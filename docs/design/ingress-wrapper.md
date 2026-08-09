# Wrapper: Built-In Ingress Proxy

> **Current v1 contract (2026-07-22):** Wrapper supports plain HTTP,
> `tls = "cluster"` using the Sesame Ingress CA, and `tls = "explicit"`
> using the certificate/key configured on the node. Omitting `tls` means
> plain HTTP. `auto` and `acme` are deferred and rejected during route
> rebuild. Sections labelled *deferred design* preserve the original ACME
> proposal; they don't describe a command or capability that works today.

## 1. Overview

Wrapper is Reliaburger's built-in reverse proxy for external traffic. It's compiled into the single `reliaburger` binary and runs on every node by default, consistent with the homogeneous node design. There's no separate install step, no IngressClass resource, no annotations, and no external cert-manager deployment.

Wrapper provides:

- **TLS termination** with cluster-issued or operator-supplied certificates
- **Host-based and path-based routing** to backend apps
- **Health-check-aware load balancing**, only routing to instances that pass health checks
- **Connection draining** during rolling deploys, letting in-flight requests complete before old instances stop
- **WebSocket support** with transparent upgrade handling for `Connection: Upgrade` requests
- **Basic rate limiting** with per-IP and per-route token bucket rate limiting to absorb traffic spikes

Wrapper runs as a set of async tasks inside the Bun agent process (not a separate process). When enabled, it binds to ports 80 and 443 on the host network namespace and proxies incoming requests to backend containers identified by their dynamically allocated host ports. The routing table is derived from the service map. When instances are added, removed, or rescheduled, Wrapper rebuilds its routing table.

Wrapper is disabled by default. Operators enable it on selected nodes with `[ingress] enabled = true` in `node.toml`. An external load balancer (cloud LB, DNS round-robin, or BGP anycast) can sit in front of those nodes.

---

## 2. Dependencies

| Dependency | Role | Failure Impact |
|---|---|---|
| **Bun agent** | Lifecycle host. Wrapper runs as async tasks within the Bun process. Bun starts Wrapper after the node has joined the cluster and the service map is initialised. | If Bun crashes, Wrapper stops. Bun's watchdog restarts the entire agent within seconds. |
| **Sesame (PKI)** | Provides Ingress CA signing material to the resolver on a council-enabled ingress node for `tls = "cluster"` routes. | Without a usable resolver, Wrapper falls back to its development self-signed listener certificate. That isn't a production substitute for a cluster certificate. |
| **Reporting tree** | Delivers runtime state (which app instances are running, on which nodes, at which host ports, and their health status) from Bun agents through council members to the leader, and back down to each node. Wrapper reads this state to build its routing table. | If the reporting tree stalls, Wrapper continues routing with the last known good routing table. Stale backends are detected by Wrapper's own active health probes. |
| **Onion (service map)** | The in-kernel BPF hash map that maps app names to lists of healthy backend `(host_ip, host_port)` entries. Wrapper reads this map to resolve which backends are available for each ingress route. | The service map is maintained by Bun locally on each node. It is always available as long as Bun is running. |

**Startup order within Bun:**

```
1. Bun joins cluster (mTLS handshake via Sesame node certificate)
2. Bun populates the service map from reporting tree state
3. Bun starts Wrapper listener tasks
4. Wrapper loads an operator certificate or builds its in-memory Ingress CA resolver
5. Wrapper binds ports 80 and 443
6. Wrapper begins accepting connections
```

Wrapper doesn't accept connections until at least one routing table entry exists. If no ingress routes are configured in any app spec, Wrapper binds the ports but returns `503 Service Unavailable` for all requests (with a human-readable body indicating no ingress routes are configured).

---

## 3. Architecture

### 3.1 Listener Architecture

Wrapper binds two TCP listeners on the host network namespace:

- **Port 80 (HTTP)**: Serves plain-HTTP routes and issues HTTP 308 redirects for every path on routes that require TLS. There is no ACME challenge exception in v1.
- **Port 443 (HTTPS)**: The primary listener. Terminates TLS using `rustls`, performs SNI-based certificate selection, and routes the decrypted request to the appropriate backend.

Both ports are configurable via `node.toml`:

```toml
[ingress]
http_port = 80
https_port = 443
```

Each listener spawns a tokio task per accepted connection. Connections are tracked in a `DashMap<ConnectionId, ConnectionState>` for drain coordination.

```
                    ┌─────────────────────────────────────────────┐
                    │                   Node                      │
                    │                                             │
  Port 80 ────────►│  HTTP Listener                              │
                    │    ├─ plain-HTTP routes                     │
                    │    └─ 308 redirect → HTTPS for TLS routes   │
                    │                                             │
  Port 443 ───────►│  HTTPS Listener (rustls)                    │
                    │    ├─ SNI → certificate selection            │
                    │    ├─ Host header → route lookup             │
                    │    ├─ Path matching → backend selection      │
                    │    └─ Proxy to backend (host_ip:host_port)   │
                    │         │                                    │
                    │         ├──► web-1 @ 127.0.0.1:31247        │
                    │         ├──► web-2 @ 10.0.1.7:30112         │
                    │         └──► api-1 @ 127.0.0.1:28934        │
                    └─────────────────────────────────────────────┘
```

### 3.2 Routing Table Design

The routing table is an in-memory data structure mapping `(host, path_prefix)` pairs to backend pools. Wrapper rebuilds it whenever the service map changes (typically within seconds of an instance being added, removed, or failing a health check).

**Lookup algorithm:**

1. Extract the `Host` header (or SNI hostname from the TLS handshake if `Host` is missing).
2. Look up the host in a `HashMap<String, Vec<PathRoute>>`. This is an exact match (no wildcard host matching in v1).
3. Within the matched host, iterate `PathRoute` entries sorted by path length descending (longest prefix match). The first matching prefix wins.
4. The matched `PathRoute` contains a `BackendPool` with a list of healthy backend addresses.
5. Select a backend using weighted round-robin (default) or least-connections.

**Routing table updates:**

Bun writes service map changes as they arrive from the reporting tree. Wrapper subscribes to a `tokio::sync::watch` channel that Bun publishes to whenever the routing-relevant subset of the service map changes. On each notification, Wrapper rebuilds the affected `BackendPool` entries. The rebuild is O(routes) and takes microseconds for typical clusters (hundreds of routes). During rebuild, the old routing table continues serving requests. The swap is atomic (Arc swap).

### 3.3 Backend Health Tracking

Wrapper integrates two sources of health information:

1. **Passive health (from reporting tree):** The service map already excludes instances that have failed their application-level health checks. Wrapper inherits this by reading the service map.

2. **Active health (Wrapper-local):** Wrapper performs its own lightweight L7 health probes to backends every 5 seconds (configurable). This catches cases where an instance is technically "healthy" from the app perspective but unreachable from this specific node (e.g., network partition, firewall rule, host port conflict). An active probe failure marks the backend as locally unhealthy in the `BackendPool` without affecting the cluster-wide service map.

Wrapper only routes to a backend if it's healthy in both the service map (passive) AND the local active probe (active). When all backends in a pool are unhealthy, Wrapper returns `502 Bad Gateway`.

### 3.4 TLS Certificate Management

Wrapper manages TLS certificates per ingress route. Each route specifies a TLS mode:

| Mode | Source | Use Case |
|---|---|---|
| `"cluster"` | Ingress CA (Sesame's intermediate CA dedicated to ingress) | Internal services, air-gapped environments, or services where clients trust the cluster root CA |
| `"explicit"` | Certificate and key paths from the node's `[ingress]` configuration | Public services where the operator already has a trusted certificate |
| omitted, `"none"`, `"off"`, or `"disabled"` | No certificate | Deliberately plain-HTTP routes |

`auto`, `acme`, and unknown values are configuration errors. Wrapper refuses
the route rather than guessing or falling back to plaintext.

**Cluster CA flow (current):**

```
1. Bun reconstructs the Ingress CA signing material from council state
2. Wrapper installs an SNI certificate resolver
3. The first handshake for a hostname that has a configured ingress route
   generates and signs a leaf certificate
4. Wrapper caches that certified key in memory for later handshakes
```

The resolver mints and caches a cluster leaf **only for a hostname that
currently has an ingress route**. That host allowlist is a security boundary:
without it, an attacker-chosen SNI would drive unbounded Ingress-CA signing and
grow the in-memory cache without limit. An SNI for any other host — and a
handshake with no SNI at all — is served the self-signed development
certificate instead; a client that validates the hostname rejects it, so an
unconfigured host can never obtain a trusted cluster certificate. The cache is
additionally capped as defence in depth.

This currently requires the ingress node to have council state and the
wrapping key material. If it doesn't, the listener uses its self-signed
development certificate. Operators must not treat that fallback as a
cluster-issued production certificate.

**Explicit flow (current):**

```
1. The operator sets both `tls_cert` and `tls_key` in `[ingress]`
2. Wrapper reads the PEM files during startup
3. Routes marked `tls = "explicit"` use that listener certificate
```

**Certificate storage:**

Cluster-issued leaf certificates and keys live in the resolver's memory cache.
They are regenerated after process restart. Operator-supplied material remains
in the configured files and follows the operator's file permissions and
rotation process.

**Certificate renewal:**

Automatic renewal and hot reload aren't implemented. The current in-memory
resolver also doesn't replace a cached cluster leaf at expiry. Production
acceptance therefore needs renewal before the cluster mode can be called
hands-off; until then, restart Wrapper before expiry or use externally rotated
operator material.

### 3.5 Connection Draining During Deploys

During a rolling deploy (Section 13 of the whitepaper), Wrapper coordinates with Bun to drain connections gracefully:

```
Step 1: Bun starts new instance (v2) with a new host port
        Bun waits for health check to pass

Step 2: Wrapper adds v2 to the backend pool for the route
        Wrapper removes v1 from the backend pool (stops sending NEW requests to v1)
        Existing connections to v1 continue to be served

Step 3: Wrapper waits for all in-flight connections to v1 to complete
        (subject to drain_timeout, default 30s)

Step 4: After drain completes (or timeout expires), Bun stops v1
        v1's host port is released
```

Drain coordination is event-driven: Bun publishes a `DrainBackend { app, instance_id, deadline }` event on an internal channel. Wrapper moves the backend from the `active` set to the `draining` set. New requests are never routed to draining backends. When the last in-flight connection to the draining backend closes (or the drain timeout expires), Wrapper publishes a `DrainComplete { app, instance_id }` acknowledgment, and Bun proceeds to stop the old instance.

If the drain timeout expires with connections still active, Wrapper forcibly closes the remaining connections by sending a TCP RST. This is a last resort; the 30-second default timeout is generous for most HTTP request/response cycles.

---

## 4. Data Structures

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;
use arc_swap::ArcSwap;

/// Top-level routing table. Swapped atomically via ArcSwap.
pub struct RoutingTable {
    /// Host → list of path routes, sorted by path length descending.
    pub routes: HashMap<String, Vec<PathRoute>>,
    /// Monotonic generation counter. Incremented on every rebuild.
    pub generation: u64,
    /// Timestamp of the last rebuild.
    pub last_updated: Instant,
}

/// A single path-prefix route within a host.
pub struct PathRoute {
    /// Path prefix to match (e.g., "/v1", "/api"). Empty string matches all paths.
    pub path_prefix: String,
    /// The app name this route belongs to (e.g., "web", "api").
    pub app_name: String,
    /// Pool of backends for this route.
    pub backend_pool: BackendPool,
    /// Rate limiting configuration for this route (if any).
    pub rate_limit: Option<RateLimitConfig>,
    /// Whether WebSocket upgrade is permitted on this route.
    pub websocket_enabled: bool,
    /// Headers to add/remove on proxied requests.
    pub header_rules: Vec<HeaderRule>,
}

/// A set of healthy backends for a single route.
pub struct BackendPool {
    /// Active backends that can receive new requests.
    pub active: Vec<Backend>,
    /// Backends in drain state: serving in-flight requests only.
    pub draining: Vec<DrainingBackend>,
    /// Load balancing strategy.
    pub lb_strategy: LoadBalanceStrategy,
    /// Round-robin counter (atomic, wraps around).
    pub rr_counter: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Copy)]
pub enum LoadBalanceStrategy {
    /// Weighted round-robin (default). Weights derived from instance resource allocation.
    RoundRobin,
    /// Route to the backend with the fewest active connections.
    LeastConnections,
    /// Consistent hashing on a request attribute (e.g., client IP, header value).
    ConsistentHash,
}

/// A single backend instance.
pub struct Backend {
    /// Network address of the backend (host_ip:host_port).
    pub addr: SocketAddr,
    /// Unique instance identifier (e.g., "web-3").
    pub instance_id: String,
    /// Node the backend is running on.
    pub node_id: String,
    /// Whether the local active health probe considers this backend healthy.
    pub locally_healthy: bool,
    /// Timestamp of the last successful active health probe.
    pub last_health_probe: Option<Instant>,
    /// Number of currently active connections to this backend.
    pub active_connections: std::sync::atomic::AtomicU32,
    /// Weight for weighted round-robin (default: 1).
    pub weight: u16,
}

/// A backend that is being drained (no new requests, in-flight only).
pub struct DrainingBackend {
    pub backend: Backend,
    /// When the drain was initiated.
    pub drain_started: Instant,
    /// Hard deadline after which remaining connections are RST'd.
    pub drain_deadline: Instant,
}

/// Connection drain coordination state.
pub struct ConnectionDrainState {
    /// Map of instance_id → drain info for all currently draining backends.
    pub draining: HashMap<String, DrainInfo>,
}

pub struct DrainInfo {
    /// The instance being drained.
    pub instance_id: String,
    /// App name.
    pub app_name: String,
    /// Number of in-flight connections still active.
    pub in_flight: std::sync::atomic::AtomicU32,
    /// When the drain was requested.
    pub started: Instant,
    /// Hard deadline (started + drain_timeout).
    pub deadline: Instant,
    /// Channel to notify Bun when drain completes.
    pub completion_tx: tokio::sync::oneshot::Sender<()>,
}

/// A tracked connection (for drain accounting and metrics).
pub struct TrackedConnection {
    pub id: u64,
    /// Which backend this connection is proxying to.
    pub backend_instance_id: String,
    /// When the connection was accepted.
    pub accepted_at: Instant,
    /// Whether this is a WebSocket connection (long-lived).
    pub is_websocket: bool,
    /// Bytes sent to client.
    pub bytes_tx: u64,
    /// Bytes received from client.
    pub bytes_rx: u64,
}

/// Complete ingress route specification (parsed from app TOML).
pub struct IngressRoute {
    /// The app this route belongs to.
    pub app_name: String,
    /// Hostname to match (e.g., "myapp.com").
    pub host: String,
    /// Optional path prefix (e.g., "/v1"). Defaults to "/".
    pub path: String,
    /// TLS mode for this route.
    pub tls_mode: TlsMode,
    /// Rate limiting config (if specified in app TOML).
    pub rate_limit: Option<RateLimitConfig>,
    /// Whether to enable WebSocket upgrades.
    pub websocket: bool,
}

#[derive(Clone, Copy)]
pub enum TlsMode {
    /// Plain HTTP.
    Disabled,
    /// Cluster Ingress CA — for internal services or air-gapped environments.
    Cluster,
    /// Operator-supplied certificate and key.
    Explicit,
}

/// Per-route rate limiting configuration.
#[derive(Clone)]
pub struct RateLimitConfig {
    /// Maximum requests per second per client IP.
    pub requests_per_second: f64,
    /// Burst capacity (token bucket size).
    pub burst: u32,
    /// Response status code when rate limited (default: 429).
    pub status_code: u16,
    /// Optional custom response body when rate limited.
    pub retry_after_header: bool,
}

/// Per-client-IP rate limiter state (token bucket).
pub struct RateLimiterState {
    /// Map of client IP → token bucket.
    pub buckets: dashmap::DashMap<std::net::IpAddr, TokenBucket>,
    /// Last time expired buckets were garbage-collected.
    pub last_gc: Instant,
}

pub struct TokenBucket {
    pub tokens: f64,
    pub last_refill: Instant,
    pub config: RateLimitConfig,
}

/// Header manipulation rule.
#[derive(Clone)]
pub enum HeaderRule {
    /// Add or overwrite a header on the proxied request.
    Set { name: String, value: String },
    /// Remove a header from the proxied request.
    Remove { name: String },
}

/// Active health probe configuration.
pub struct HealthProbeConfig {
    /// Interval between probes.
    pub interval: Duration,
    /// Timeout for each probe.
    pub timeout: Duration,
    /// Number of consecutive failures before marking unhealthy.
    pub threshold_unhealthy: u32,
    /// Number of consecutive successes before marking healthy again.
    pub threshold_healthy: u32,
    /// HTTP path to probe (default: "/").
    pub path: String,
    /// Expected HTTP status codes (default: 200-399).
    pub expected_status: std::ops::RangeInclusive<u16>,
}

/// Global Wrapper configuration (from node.toml [ingress] section).
pub struct WrapperConfig {
    /// Whether Wrapper is enabled on this node.
    pub enabled: bool,
    /// HTTP listener port (default: 80).
    pub http_port: u16,
    /// HTTPS listener port (default: 443).
    pub https_port: u16,
    /// Operator-supplied certificate PEM path.
    pub tls_cert_path: Option<PathBuf>,
    /// Operator-supplied private-key PEM path.
    pub tls_key_path: Option<PathBuf>,
    /// Default drain timeout for rolling deploys.
    pub drain_timeout: Duration,
    /// Global default rate limit (applied to routes without explicit config).
    pub default_rate_limit: Option<RateLimitConfig>,
    /// Active health probe configuration.
    pub health_probe: HealthProbeConfig,
    /// Minimum TLS version (default: TLS 1.2).
    pub min_tls_version: TlsVersion,
}

#[derive(Clone, Copy)]
pub enum TlsVersion {
    Tls12,
    Tls13,
}
```

---

## 5. Operations

### 5.1 Request Routing

Every inbound HTTPS request follows this path:

```
1.  TCP accept on port 443
2.  TLS handshake (rustls)
    - SNI hostname extracted
    - A cluster leaf is minted/served only for a configured ingress host;
      an unknown SNI (or none) is served the self-signed default, which a
      hostname-validating client rejects
3.  HTTP/1.1 or HTTP/2 request parsed (via hyper)
4.  Host header extracted (falls back to SNI hostname)
5.  Routing table lookup:
    a. Exact match on Host → Vec<PathRoute>
    b. Longest path prefix match → PathRoute
    c. If no match: respond 404 Not Found
6.  Rate limit check:
    a. Extract client IP (from X-Forwarded-For if trusted proxy, else peer IP)
    b. Check token bucket for (client_ip, route)
    c. If rate limited: respond 429 Too Many Requests with Retry-After header
7.  Backend selection from BackendPool:
    a. Filter to locally_healthy == true
    b. If pool empty: respond 502 Bad Gateway
    c. Select backend per lb_strategy (default: weighted round-robin)
8.  Proxy the request:
    a. Add X-Forwarded-For, X-Forwarded-Proto, X-Real-IP headers
    b. Forward the request to backend_addr
    c. Stream the response back to the client
9.  Connection accounting:
    a. Increment active_connections on the selected backend
    b. On response completion: decrement active_connections
    c. If backend is draining and active_connections reaches 0: signal DrainComplete
```

### 5.2 TLS Modes

**Mode: `cluster`**

Used where clients trust the cluster root. On an ingress node with reconstructed
Ingress CA material, the SNI resolver issues and caches a hostname certificate
locally. There is no network CSR round-trip in the current implementation.

**Mode: `explicit`**

Used with `tls_cert` and `tls_key` in the node's `[ingress]` configuration.
This is the public-certificate path in v1. The same listener certificate serves
all explicit routes on that node, so its SANs must cover every advertised host.

**Mode: omitted / `none` / `off` / `disabled`**

The application route is served as plain HTTP. There is deliberately no
implicit TLS default.

### 5.3 ACME Certificate Provisioning (deferred design)

`auto` and `acme` are reserved but rejected. A future implementation needs an
explicit issuer model, production/staging directory selection, account-key
ownership, challenge lifecycle, rate-limit coordination, renewal, persistence,
revocation, multi-node distribution and hermetic acceptance tests. Until all of
that exists, Wrapper doesn't expose port 80 as an HTTP-01 challenge responder.

If we implement it, the leader should lease each hostname to one challenge
solver and distribute the resulting certificate to ingress nodes. DNS-01
provider integrations should be separate optional adapters; claiming support
for Cloudflare, Route53 or Google Cloud DNS before those adapters and their
credential boundaries exist would repeat the same documentation mistake.

### 5.4 Certificate Renewal (required follow-up)

The v1 cluster resolver caches a generated leaf for the life of the Bun process
and doesn't inspect its expiry. Operator certificates load at startup and don't
hot-reload. A production implementation must renew or reload before expiry,
retain the last valid certificate while rotation runs, expose expiry telemetry,
and fail acceptance when a required certificate can't be refreshed.

### 5.5 Connection Draining

Connection draining is the mechanism that ensures zero-downtime rolling deploys. The drain protocol between Wrapper and Bun:

```
Bun (deploy coordinator)                  Wrapper
         │                                    │
         │ DrainBackend{instance_id, timeout}  │
         │──────────────────────────────────►  │
         │                                    │  Move backend from active → draining
         │                                    │  Stop routing NEW requests to backend
         │                                    │  Wait for in_flight connections to close
         │                                    │
         │                    (connections close naturally)
         │                                    │
         │  DrainComplete{instance_id}        │
         │◄──────────────────────────────────  │
         │                                    │
         │ (Bun stops the old container)      │
```

**Drain timeout behaviour:**

- Default: 30 seconds (configurable per app via `drain_timeout` in the deploy config).
- When the timeout expires with connections still active:
  1. WebSocket connections: Wrapper sends a WebSocket Close frame (opcode 0x08) with status 1001 (Going Away), then waits 5 seconds for the close handshake, then RSTs.
  2. HTTP connections: Wrapper sends a 503 response if the request is mid-stream, then RSTs.
  3. Idle keep-alive connections: RST immediately.
- The drain timeout is a per-deploy-step timeout, not a global timeout. Each instance being replaced gets its own full drain window.

**Coordination with rolling deploys:**

The rolling deploy process (Section 13) proceeds one instance at a time (configurable via `max_surge`). Wrapper's drain is step 2 of each instance replacement. The deploy doesn't proceed to the next instance until the current drain is complete (or timed out). This ensures that the app never drops below `replicas - max_surge` healthy instances at any point during the deploy.

### 5.6 WebSocket Upgrade Handling

> **Status:** Deferred to Phase 9 (User Experience). The `websocket` config flag is parsed but the proxy currently handles HTTP only. The upgrade handshake and bidirectional byte-level proxying described below are not yet implemented.

When Wrapper receives a request with `Connection: Upgrade` and `Upgrade: websocket` headers:

1. Wrapper validates the `Sec-WebSocket-Key` header is present.
2. Wrapper selects a backend from the pool (same algorithm as HTTP).
3. Wrapper forwards the upgrade request to the backend.
4. If the backend responds with `101 Switching Protocols`, Wrapper enters bidirectional byte-level proxying (no HTTP framing).
5. The connection is tracked as `is_websocket: true` in the connection tracker.
6. WebSocket connections are long-lived. During connection draining, they receive the Close frame treatment described above.

Wrapper doesn't inspect or modify WebSocket frames. It operates as a transparent TCP proxy after the upgrade handshake.

### 5.7 Rate Limiting

Rate limiting uses a per-client-IP token bucket algorithm:

```rust
fn check_rate_limit(
    state: &RateLimiterState,
    client_ip: IpAddr,
    config: &RateLimitConfig,
) -> Result<(), Duration> {
    let mut bucket = state.buckets
        .entry(client_ip)
        .or_insert_with(|| TokenBucket {
            tokens: config.burst as f64,
            last_refill: Instant::now(),
            config: config.clone(),
        });

    let elapsed = bucket.last_refill.elapsed().as_secs_f64();
    bucket.tokens = (bucket.tokens + elapsed * config.requests_per_second)
        .min(config.burst as f64);
    bucket.last_refill = Instant::now();

    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        Ok(())
    } else {
        let wait = Duration::from_secs_f64(
            (1.0 - bucket.tokens) / config.requests_per_second
        );
        Err(wait) // Retry-After duration
    }
}
```

Rate limiter state is per-node (not cluster-wide). Each node independently rate-limits based on the traffic it receives. This is simple and avoids distributed state, but means that a client hitting N nodes gets N times the rate limit. For most deployments behind an external load balancer, this is acceptable because the LB pins clients to nodes.

**Garbage collection:** Every 60 seconds, a background task removes token buckets that haven't been accessed in the last 5 minutes, preventing unbounded memory growth from unique client IPs.

Per-route rate limits are configured in the app spec:

```toml
[app.api.ingress]
host = "api.myapp.com"
path = "/v1"
tls = "cluster"
rate_limit_rps = 100
rate_limit_burst = 200
```

If no per-route limit is configured, the global default from `node.toml` applies. If no global default is configured, no rate limiting is applied.

### 5.8 Routing Table Updates from Reporting Tree

The reporting tree delivers state changes through this pipeline:

```
Backend instance starts/stops/fails health check on some node
  → That node's Bun agent reports to its council member
  → Council member aggregates and reports to the leader
  → Leader disseminates updated state back down the tree
  → Each node's Bun agent receives the update
  → Bun updates the local service map (BPF hash map)
  → Bun publishes a notification on the watch channel
  → Wrapper receives the notification
  → Wrapper rebuilds affected BackendPool entries
  → New routing table is swapped in via ArcSwap
```

End-to-end latency for a routing table update: typically 1-3 seconds, dominated by the reporting tree aggregation interval. During this window, Wrapper may still route to a backend that has just become unhealthy. The active health probe (5-second interval) provides a secondary safety net.

---

## 6. Configuration

### 6.1 Node-Level Configuration (`node.toml`)

```toml
[ingress]
# Whether Wrapper is enabled on this node. Default: false.
enabled = true

# HTTP listener port. Serves plain routes and HTTPS redirects.
# Default: 80.
http_port = 80

# HTTPS listener port. Primary traffic port.
# Default: 443.
https_port = 443

# Optional operator-supplied PEM files. Configure both or neither.
# Required by routes using tls = "explicit".
tls_cert = "/etc/reliaburger/ingress/fullchain.pem"
tls_key = "/etc/reliaburger/ingress/private-key.pem"

# Default drain timeout for rolling deploys.
# Per-app drain_timeout in the app spec overrides this.
# Default: "30s".
drain_timeout = "30s"

# Global default rate limit (requests per second per client IP).
# Applied to routes that do not specify their own rate limit.
# Default: none (no rate limiting).
# rate_limit_rps = 1000
# rate_limit_burst = 2000

# Minimum TLS version. Default: "1.2".
# Set to "1.3" to disable TLS 1.2.
min_tls_version = "1.2"

# Active health probe interval. Default: "5s".
health_probe_interval = "5s"

# Active health probe timeout. Default: "2s".
health_probe_timeout = "2s"

# Consecutive probe failures before marking backend unhealthy. Default: 3.
health_threshold_unhealthy = 3

# Consecutive probe successes before marking backend healthy. Default: 2.
health_threshold_healthy = 2
```

### 6.2 App-Level Configuration (app spec TOML)

```toml
[app.web.ingress]
# Hostname to route to this app. Required.
host = "myapp.com"

# Path prefix to match. Default: "/" (match all paths).
path = "/"

# TLS mode. Omit for plain HTTP; use "cluster" or "explicit" for HTTPS.
tls = "explicit"

# Whether to allow WebSocket upgrades on this route. Default: false.
websocket = false

# Per-route rate limit (requests per second per client IP). Default: none.
# rate_limit_rps = 100
# rate_limit_burst = 200

[app.web.deploy]
# Drain timeout for this app during rolling deploys. Default: node-level setting.
drain_timeout = "30s"
```

### 6.3 Configuration Validation

Wrapper validates ingress configuration at deploy time:

- **Duplicate host/path:** If two apps declare the same `(host, path)` combination, the deploy is rejected with a clear error message.
- **Unsupported TLS mode:** `auto`, `acme`, and unknown values reject the routing-table rebuild. Supported values are `cluster`, `explicit`, and the plain-HTTP aliases.
- **Invalid hostname:** Hostnames are validated against RFC 952 (alphanumeric, hyphens, dots, no wildcards in v1).
- **Path format:** Paths must start with `/` and not contain query strings or fragments.

---

## 7. Failure Modes

| Failure | Detection | Impact | Recovery |
|---|---|---|---|
| **Unsupported TLS mode** | Routing-table rebuild parses `auto`, `acme`, or an unknown value | The new routing table is rejected; the previous table remains active. | Choose `cluster`, `explicit`, or a plain-HTTP alias. |
| **Backend pool empty** | All backends removed from service map or all fail active health probes | Route returns 502 Bad Gateway for all requests. | Automatic: backends re-appear when health checks pass or new instances are scheduled. Wrapper re-adds them within seconds. |
| **Certificate expiry** | Currently detected by clients, not by Wrapper | Clients reject the connection. | Restart before a cluster-issued leaf expires, or rotate the configured operator files and restart. Automatic detection, telemetry and hot rotation are required follow-up work. |
| **Slow draining** | In-flight connections exceed `drain_timeout` | Deploy step is delayed up to `drain_timeout`. After timeout, remaining connections are forcibly closed (TCP RST). | Increase `drain_timeout` if the app has legitimately long-running requests. For WebSocket apps, set a higher timeout or implement reconnection logic in the client. |
| **Ingress CA resolver unavailable at startup** | Bun can't reconstruct the Ingress CA material and logs a warning | The HTTPS listener uses its self-signed development certificate; `cluster` doesn't meet its production trust contract on that node. | Restore council/wrapping material before enabling ingress, or configure an explicit certificate pair. A future capability gate should reject placement on such a node. |
| **Port 80/443 already in use** | `bind()` returns `EADDRINUSE` | Wrapper cannot start. Bun logs the error and retries every 30 seconds. | Operator must free the ports or reconfigure Wrapper to use alternative ports. |
| **rustls handshake failure** | Client sends unsupported TLS version or cipher suite | Connection dropped during handshake | Client-side fix (upgrade TLS version). Wrapper logs the failure at debug level to avoid log flooding. |
| **Upstream connection refused** | Backend process crashed between health probe and request routing | Individual request fails with 502. Wrapper marks backend as locally unhealthy after `threshold_unhealthy` consecutive failures. | Automatic: backend removed from pool. Next request goes to a healthy backend. Bun's container supervision restarts the crashed process. |

---

## 8. Security Considerations

### 8.1 TLS Configuration

Wrapper uses `rustls` (a memory-safe TLS implementation) with the following defaults:

- **Minimum TLS version:** TLS 1.2 (configurable to TLS 1.3 only via `min_tls_version = "1.3"`).
- **TLS 1.3 cipher suites (preferred):**
  - `TLS_AES_256_GCM_SHA384`
  - `TLS_AES_128_GCM_SHA256`
  - `TLS_CHACHA20_POLY1305_SHA256`
- **TLS 1.2 cipher suites (when TLS 1.2 is enabled):**
  - `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`
  - `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`
  - `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`
  - `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`
  - `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`
  - `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`
- **ECDH groups:** X25519, secp256r1, secp384r1.
- **No support for:** TLS 1.0, TLS 1.1, RC4, 3DES, CBC-mode ciphers, RSA key exchange (non-ECDHE). These are structurally impossible with rustls, which doesn't implement them.

Wrapper doesn't currently fetch or staple OCSP responses. Cluster-issued certificates have no public OCSP responder. Operators using explicit certificates must account for the client behaviour they require.

### 8.2 ACME Account Security (deferred design)

There is no ACME account or `relish ingress acme-deactivate` command in v1.
A future implementation must define who owns the account key, how Raft state
encrypts and rotates it, which nodes may solve challenges, and how an operator
revokes a compromised account without distributing the key to every worker.

### 8.3 Rate Limiting Against DDoS

Wrapper's rate limiting is a first line of defense, not a complete DDoS mitigation solution. It protects backends from moderate traffic spikes and prevents a single client IP from monopolizing resources.

Limitations:

- Per-node, not cluster-wide. A distributed attack hitting all nodes bypasses per-node limits.
- IP-based only. Cannot rate limit by API key, user identity, or other application-level attributes.
- Doesn't protect against volumetric attacks that saturate network bandwidth before reaching Wrapper.

For production deployments facing DDoS risk, operators should place a dedicated DDoS mitigation service (Cloudflare, AWS Shield, etc.) in front of the cluster. Wrapper's rate limiting protects against application-layer abuse, not network-layer floods.

### 8.4 Header Security

Wrapper adds the following headers to proxied requests:

- `X-Forwarded-For`: Client IP appended to any existing chain.
- `X-Forwarded-Proto`: `https`.
- `X-Real-IP`: Client's direct IP address.
- `X-Request-ID`: Unique request identifier (UUID v4) if not already present.

Wrapper strips the following headers from client requests before proxying:

- `X-Forwarded-For` (replaced with accurate value to prevent spoofing, unless the peer IP is in a configured trusted proxy CIDR range).

---

## 9. Performance

### 9.1 Request Latency Overhead

Wrapper adds latency at two points:

| Phase | Expected Overhead | Notes |
|---|---|---|
| TLS handshake (full) | 1-3 ms | Dominated by ECDHE key exchange. TLS 1.3 requires one fewer round-trip than TLS 1.2. Amortized to zero for keep-alive connections. |
| TLS handshake (resumption) | 0.1-0.5 ms | Session tickets (TLS 1.3 PSK) are enabled by default. |
| HTTP parsing + routing | 10-50 us | In-memory routing table lookup. Negligible for typical route counts (<1000). |
| Proxy overhead (per request) | 50-200 us | Memory copies between client and backend sockets. hyper's zero-copy streaming minimizes this. |
| Rate limit check | 1-5 us | DashMap lookup + token bucket arithmetic. |

**Total added latency per request (steady state, keep-alive connection):** 60-250 microseconds. This is well below the typical application response time (1-100 ms) and isn't a practical bottleneck.

### 9.2 Concurrent Connection Capacity

Wrapper uses tokio's async I/O model. Each connection is a lightweight task (~256 bytes of stack), not an OS thread. Expected capacity:

- **10,000 concurrent connections per node** with no special tuning (default tokio runtime).
- **100,000+ concurrent connections per node** with OS-level tuning (`ulimit -n`, `net.core.somaxconn`, `net.ipv4.tcp_max_syn_backlog`).

Connection memory overhead: approximately 8 KB per connection (TLS session state + hyper buffers + tracking metadata). At 100,000 connections, this is ~800 MB of memory.

### 9.3 TLS Handshake Cost

TLS handshakes are CPU-intensive (ECDHE key exchange). Approximate throughput on a modern CPU core:

- **ECDHE-P256 full handshakes:** ~5,000/second per core
- **TLS 1.3 PSK resumption:** ~20,000/second per core
- **X25519 key exchange:** ~8,000/second per core

For most deployments, keep-alive connections amortize the handshake cost. The handshake rate becomes a bottleneck only under connection storms (e.g., CDN cache purge causing thousands of new connections per second). TLS 1.3 session tickets are enabled by default to minimise full handshakes.

### 9.4 Routing Table Rebuild Cost

Routing table rebuilds (triggered by service map changes) are O(n) where n is the number of ingress routes. For a cluster with 500 routes, the rebuild takes ~50 microseconds. The ArcSwap ensures that in-flight requests are never blocked by a rebuild.

---

## 10. Testing Strategy

### 10.1 TLS Termination Testing

| Test Case | Method |
|---|---|
| TLS 1.3 handshake completes successfully | Integration test: connect with rustls client configured for TLS 1.3 only. Verify connection succeeds and negotiated protocol is TLS 1.3. |
| TLS 1.2 handshake completes when permitted | Integration test: connect with TLS 1.2 client. Verify success when `min_tls_version = "1.2"`, failure when `min_tls_version = "1.3"`. |
| TLS 1.0/1.1 rejected | Integration test: connect with TLS 1.0/1.1 client. Verify handshake failure. |
| SNI-based certificate selection | Integration test: configure two routes with different hostnames and certificates. Connect with different SNI values. Verify correct certificate is served. |
| Unconfigured / missing SNI | Connect with an SNI that has no route, and with no SNI. Verify the self-signed default is served (not a minted cluster leaf) and nothing is cached, so an arbitrary SNI can't drive CA signing. |
| Certificate hot-swap | Load a certificate, verify it is served. Replace with a new certificate. Verify new certificate is served without connection drops. |
| ACME HTTP-01 challenge response *(deferred)* | Acceptance test must prove the challenge is served only while a valid lease exists and ordinary TLS routes don't gain a permanent plaintext bypass. |
| Cluster CA certificate signing | Integration test: submit a CSR to a mock council member. Verify the returned certificate is valid and signed by the Ingress CA. |
| Expired certificate detection | Unit test: load a certificate with `not_after` in the past. Verify renewal is triggered immediately. |
| Certificate cache persistence | Integration test: provision a certificate, restart Wrapper, verify the certificate is loaded from disk without re-provisioning. |

### 10.2 Routing Correctness

| Test Case | Method |
|---|---|
| Exact host match | Request to `Host: myapp.com` routes to the correct backend pool. |
| Host mismatch returns 404 | Request to `Host: unknown.com` returns 404. |
| Longest path prefix match | Routes `/api` and `/api/v2` both exist. Request to `/api/v2/users` matches `/api/v2`, not `/api`. |
| Root path match | Route with `path = "/"` matches `/anything`. |
| Backend round-robin | Send N requests. Verify backends are selected in round-robin order. |
| Backend least-connections | Configure `lb_strategy = LeastConnections`. Send concurrent requests. Verify backends are selected by lowest active connection count. |
| Empty backend pool → 502 | Remove all backends from a route. Verify 502 response. |
| Routing table update | Add a new backend to the service map. Verify it starts receiving requests within 5 seconds. |
| Concurrent routing table swap | Send a continuous stream of requests while triggering a routing table rebuild. Verify no requests are dropped or return errors. |

### 10.3 Drain Behaviour Verification

| Test Case | Method |
|---|---|
| Graceful drain completes | Start a slow request (5-second response time). Initiate drain. Verify the slow request completes. Verify DrainComplete is signaled after the response finishes. |
| Drain timeout forces RST | Start a request that never completes (blocked server). Initiate drain with 2-second timeout. Verify the connection is RST after 2 seconds. Verify DrainComplete is signaled. |
| No new requests to draining backend | Initiate drain on a backend. Send 100 new requests. Verify zero requests reach the draining backend. |
| WebSocket drain sends Close frame | Establish a WebSocket connection. Initiate drain. Verify the client receives a Close frame with status 1001 before disconnect. |
| Rolling deploy end-to-end | Deploy a new version of an app with 3 replicas. Send continuous traffic during the deploy. Verify zero failed requests (5xx responses). Verify all instances are eventually replaced. |

### 10.4 Rate Limiting

| Test Case | Method |
|---|---|
| Under limit: all requests pass | Send 50 RPS with a 100 RPS limit. Verify zero 429 responses. |
| Over limit: excess requests rejected | Send 200 RPS with a 100 RPS limit. Verify approximately 50% of requests receive 429. |
| Burst capacity | Send 200 requests simultaneously with burst=200, rps=100. Verify all 200 pass. Immediately send 1 more. Verify 429. |
| Retry-After header | Trigger a 429. Verify `Retry-After` header is present and contains a valid duration. |
| Per-IP isolation | Two clients at different IPs. One exceeds the limit. Verify the other is unaffected. |
| Bucket garbage collection | Send requests from 10,000 unique IPs. Wait 5 minutes. Verify memory is reclaimed. |

### 10.5 WebSocket

| Test Case | Method |
|---|---|
| Upgrade handshake success | Send an HTTP Upgrade request. Verify 101 response and bidirectional data flow. |
| Non-WebSocket route rejects upgrade | Send an Upgrade request to a route with `websocket = false`. Verify 400 or routing proceeds without upgrade. |
| Bidirectional data flow | Establish a WebSocket. Send data in both directions. Verify integrity. |
| Large frames | Send 64 KB WebSocket frames. Verify correct delivery. |

---

## 11. Prior Art

### 11.1 nginx

The most widely deployed reverse proxy. Configuration is file-based (declarative but static). TLS certificate management is manual or delegated to external tools (certbot). Reload (`nginx -s reload`) replaces worker processes gracefully but requires external orchestration to trigger. nginx has excellent performance characteristics but requires significant operational overhead for dynamic environments.

**What we learn:** nginx's worker-per-core architecture demonstrates that per-connection threading is unnecessary for high-concurrency proxying. Wrapper uses tokio's async model to achieve similar concurrency with less complexity.

**What we do differently:** Wrapper's routing table is dynamic (updated from the reporting tree in seconds). nginx requires a config file reload. Wrapper handles TLS provisioning natively; nginx delegates to certbot.

### 11.2 Traefik

A cloud-native reverse proxy designed for dynamic service discovery. Traefik watches orchestrator APIs (Kubernetes, Docker, Consul) and updates its routing table automatically. It supports automatic HTTPS via ACME.

- Architecture reference: [Traefik documentation](https://doc.traefik.io/traefik/)

**What we borrow:** Traefik's model of auto-discovering backends from an orchestrator's API and dynamically updating routes. Wrapper does the same, but reads from the reporting tree and service map rather than the Kubernetes API.

**What we do differently:** Traefik is a standalone process that must be deployed, configured, and updated separately. Wrapper is built into the Reliaburger binary. Wrapper has no ACME implementation in v1; any future design needs cluster coordination instead of pretending independent nodes can safely share an account and hostname.

### 11.3 HAProxy

The gold standard for high-performance load balancing. HAProxy supports advanced load balancing algorithms, connection draining, health checking, and rate limiting. Its configuration is powerful but complex.

**What we learn:** HAProxy's connection draining model (soft-stop) directly inspired Wrapper's drain protocol. HAProxy's active + passive health checking model (agent checks + HTTP checks) is the basis for Wrapper's dual health source design.

**What we do differently:** HAProxy requires manual configuration of backends. Wrapper discovers backends automatically. HAProxy doesn't handle TLS certificate provisioning.

### 11.4 Envoy

A high-performance proxy designed for service mesh architectures. Envoy uses xDS (discovery service) APIs to receive configuration dynamically from a control plane.

- Architecture reference: [Envoy xDS protocol](https://www.envoyproxy.io/docs/envoy/latest/api-docs/xds_protocol)

**What we learn:** Envoy's xDS model demonstrates the value of separating the proxy data plane from the configuration control plane. Wrapper achieves a similar separation: the reporting tree is the control plane that pushes routing state to Wrapper's data plane.

**What we do differently:** Envoy's xDS is a complex gRPC-based protocol with multiple resource types (CDS, EDS, LDS, RDS). Wrapper's configuration source is the service map, a single flat data structure already maintained by Bun. No additional protocol or API is needed.

### 11.5 Caddy

A web server with automatic HTTPS as a first-class feature. Caddy provisions TLS certificates via ACME (HTTP-01, DNS-01, TLS-ALPN-01) automatically for any hostname it serves. No configuration is required beyond specifying the hostname.

- ACME implementation reference: [Caddy's automatic HTTPS](https://caddyserver.com/docs/automatic-https)

**What we may borrow later:** Caddy demonstrates that automatic certificate lifecycle can be a good product default once its issuer, challenges, renewal and failure behaviour are complete. Wrapper doesn't expose that promise in v1.

**What we do differently:** Caddy is a standalone web server. Wrapper is embedded in the orchestrator binary and already has a `tls = "cluster"` mode for clients that trust Reliaburger's root. A future ACME design would need cluster-wide leases to avoid duplicate orders and rate-limit failures.

---

## 12. Libraries & Dependencies

All dependencies are Rust crates compiled into the single `reliaburger` binary.

| Crate | Version (min) | Purpose | Notes |
|---|---|---|---|
| [`hyper`](https://crates.io/crates/hyper) | 1.x | HTTP/1.1 and HTTP/2 server and client implementation | Used for both the listener (server) and the backend proxy connection (client). Provides streaming body support for efficient proxying. |
| [`rustls`](https://crates.io/crates/rustls) | 0.23.x | TLS implementation | Memory-safe TLS. No OpenSSL dependency. Supports TLS 1.2 and 1.3. SNI-based certificate selection via `ResolvesServerCert`. Session tickets for TLS 1.3 PSK resumption. |
| [`tokio`](https://crates.io/crates/tokio) | 1.x | Async runtime | Multi-threaded runtime with work-stealing scheduler. Provides `TcpListener`, `TcpStream`, timers, channels, and task spawning. Already used throughout the Bun agent. |
| [`instant-acme`](https://crates.io/crates/instant-acme) *(candidate, not currently a dependency)* | To decide | Deferred ACME protocol client | Reassess when automatic public certificates enter the roadmap; library choice follows the issuer and challenge design. |
| [`tokio-tungstenite`](https://crates.io/crates/tokio-tungstenite) | 0.24.x | WebSocket protocol implementation | Used for WebSocket upgrade detection and Close frame generation during connection draining. After the upgrade handshake, Wrapper uses raw TCP proxying for performance. |
| [`rcgen`](https://crates.io/crates/rcgen) | 0.13.x | X.509 certificate generation | Used for the development certificate and cluster-CA leaf certificates. |
| [`webpki`](https://crates.io/crates/webpki) *(candidate, not currently a direct dependency)* | To decide | Deferred certificate validation | Reassess with ACME and explicit-certificate validation work. |
| [`rustls-pemfile`](https://crates.io/crates/rustls-pemfile) | 2.x | PEM file parsing | Used to load certificates from disk cache and from operator-provided manual certificate files. |

---

## 13. Open Questions

### 13.1 HTTP/3 Support (QUIC)

HTTP/3 (over QUIC) provides benefits including 0-RTT connection establishment, multiplexed streams without head-of-line blocking, and connection migration across network changes. Adding HTTP/3 support to Wrapper would require:

- A QUIC implementation crate (e.g., `quinn` or `s2n-quic`).
- UDP listener on port 443 (in addition to the TCP listener).
- `Alt-Svc` response header advertising HTTP/3 availability.
- QUIC-specific connection tracking and drain logic.

**Decision status:** Deferred to a future version. HTTP/3 adoption is growing but HTTP/1.1 + HTTP/2 cover the vast majority of production traffic today. The hyper ecosystem is actively working on HTTP/3 support (`hyper` + `h3` + `quinn`), and we should adopt it once the stack stabilizes.

**Risk of deferral:** Low. No production workload currently requires HTTP/3. Clients that support HTTP/3 gracefully fall back to HTTP/2.

### 13.2 gRPC Proxying

gRPC uses HTTP/2 with specific framing conventions (trailers, streaming, content-type `application/grpc`). Wrapper's current HTTP/2 proxying may work for unary gRPC calls, but streaming gRPC (server-streaming, client-streaming, bidirectional) hasn't been validated.

Known concerns:

- gRPC trailers must be forwarded correctly (hyper handles this, but needs verification).
- gRPC client-side load balancing may conflict with Wrapper's backend selection.
- gRPC health checking protocol (`grpc.health.v1.Health`) is different from HTTP health checking.
- Long-lived gRPC streams interact with connection draining (similar to WebSocket, but using HTTP/2 GOAWAY instead of WebSocket Close).

**Decision status:** Needs investigation and testing. gRPC proxying should work with Wrapper's HTTP/2 support, but requires a dedicated test suite before being documented as supported.

### 13.3 Custom Middleware / Header Injection

Some users will want to inject custom headers (e.g., `X-Request-ID`, `X-Trace-ID`, custom authentication headers) or run custom logic (e.g., request logging, authentication, request transformation) at the ingress layer.

Options under consideration:

1. **Header rules in app spec:** Simple `add_headers` and `remove_headers` fields in the ingress config. Already partially designed in the `HeaderRule` data structure.
2. **Lua scripting:** Embed a Lua interpreter (e.g., `mlua` or `rlua`) for custom request/response processing. Precedent: nginx's `access_by_lua`, HAProxy's Lua integration.
3. **WASM plugins:** Run user-provided WebAssembly modules for request processing. Precedent: Envoy's WASM filter chain.
4. **No middleware:** Keep Wrapper simple. Custom logic belongs in the application or in a sidecar.

**Decision status:** Option 1 (header rules) is planned for v1. Options 2-4 are deferred. The principle of "do less, but do it well" suggests that Wrapper should remain a focused reverse proxy, not an extensible middleware platform.

### 13.4 Wildcard and Regex Host Matching

The current design supports exact host matching only. Some deployments need:

- Wildcard hosts: `*.myapp.com` matches `a.myapp.com`, `b.myapp.com`, etc.
- Regex paths: `/api/v[0-9]+/users` instead of prefix-only matching.

**Decision status:** Deferred. Wildcard host matching is a likely v2 addition (common use case for multi-tenant SaaS). Regex path matching adds complexity and performance cost (regex evaluation per request) and should be evaluated carefully.

### 13.5 Mutual TLS (Client Certificate Authentication)

Some internal services require clients to present a valid certificate (mTLS at the ingress layer, distinct from Sesame's inter-node mTLS). This requires:

- Configuring a trusted client CA per route.
- Extracting client identity from the certificate and passing it to the backend (e.g., via `X-Client-CN` header).

**Decision status:** Deferred. The infrastructure for this exists in Sesame's PKI hierarchy, but the ingress-layer mTLS configuration and identity extraction haven't been designed.

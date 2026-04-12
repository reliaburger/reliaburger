# Wrapper: Built-In Ingress Proxy

## 1. Overview

Wrapper is Reliaburger's built-in reverse proxy for external traffic. It's compiled into the single `reliaburger` binary and runs on every node by default, consistent with the homogeneous node design. There's no separate install step, no IngressClass resource, no annotations, and no external cert-manager deployment.

Wrapper provides:

- **TLS termination** with automatic certificate provisioning (ACME for public services, cluster Ingress CA for internal services)
- **Host-based and path-based routing** to backend apps
- **Health-check-aware load balancing**, only routing to instances that pass health checks
- **Connection draining** during rolling deploys, letting in-flight requests complete before old instances stop
- **WebSocket support** with transparent upgrade handling for `Connection: Upgrade` requests
- **Basic rate limiting** with per-IP and per-route token bucket rate limiting to absorb traffic spikes

Wrapper runs as a set of async tasks inside the Bun agent process (not a separate process). It binds to ports 80 and 443 on the host network namespace and proxies incoming requests to backend containers identified by their dynamically allocated host ports. The routing table is derived from the service map, which is populated by the hierarchical reporting tree. When instances are added, removed, or rescheduled, Wrapper updates its routing table automatically within seconds.

Operators can disable Wrapper on specific nodes via `[ingress] enabled = false` in `node.toml` (e.g., GPU-heavy compute nodes where ingress traffic would compete for resources), but the default is that every node serves ingress traffic. An external load balancer (cloud LB, DNS round-robin, or BGP anycast) can be placed in front of the cluster to distribute traffic across nodes, but isn't required.

---

## 2. Dependencies

| Dependency | Role | Failure Impact |
|---|---|---|
| **Bun agent** | Lifecycle host. Wrapper runs as async tasks within the Bun process. Bun starts Wrapper after the node has joined the cluster and the service map is initialised. | If Bun crashes, Wrapper stops. Bun's watchdog restarts the entire agent within seconds. |
| **Sesame (PKI)** | Provides the Ingress CA intermediate certificate and private key (via council members) for `tls = "cluster"` routes. Sesame's ACME account key management backs `tls = "acme"` routes. | If Sesame is unavailable, existing certificates continue serving traffic until expiry. New `tls = "cluster"` certificate signing requests queue until a council member is reachable. ACME certificates are independent of Sesame's CA hierarchy — they rely on an external ACME provider. |
| **Reporting tree** | Delivers runtime state (which app instances are running, on which nodes, at which host ports, and their health status) from Bun agents through council members to the leader, and back down to each node. Wrapper reads this state to build its routing table. | If the reporting tree stalls, Wrapper continues routing with the last known good routing table. Stale backends are detected by Wrapper's own active health probes. |
| **Onion (service map)** | The in-kernel BPF hash map that maps app names to lists of healthy backend `(host_ip, host_port)` entries. Wrapper reads this map to resolve which backends are available for each ingress route. | The service map is maintained by Bun locally on each node. It is always available as long as Bun is running. |

**Startup order within Bun:**

```
1. Bun joins cluster (mTLS handshake via Sesame node certificate)
2. Bun populates the service map from reporting tree state
3. Bun starts Wrapper listener tasks
4. Wrapper loads TLS certificates from local cache (or requests new ones)
5. Wrapper binds ports 80 and 443
6. Wrapper begins accepting connections
```

Wrapper doesn't accept connections until at least one routing table entry exists. If no ingress routes are configured in any app spec, Wrapper binds the ports but returns `503 Service Unavailable` for all requests (with a human-readable body indicating no ingress routes are configured).

---

## 3. Architecture

### 3.1 Listener Architecture

Wrapper binds two TCP listeners on the host network namespace:

- **Port 80 (HTTP)**: Serves two purposes: (1) issues HTTP 301 redirects to HTTPS for routes that have TLS enabled, and (2) responds to ACME HTTP-01 challenge requests at `/.well-known/acme-challenge/<token>`. No application traffic is served over plain HTTP.
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
                    │    ├─ ACME HTTP-01 challenges               │
                    │    └─ 301 redirect → HTTPS                  │
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
| `"acme"` | Let's Encrypt via ACME protocol (HTTP-01 or DNS-01 challenge) | Public-facing services where browsers must trust the certificate |
| `"cluster"` | Ingress CA (Sesame's intermediate CA dedicated to ingress) | Internal services, air-gapped environments, or services where clients trust the cluster root CA |
| `"auto"` | `"acme"` if the cluster has internet access; `"cluster"` if configured as air-gapped | Default when a TLS mode is not explicitly specified |

**ACME flow (HTTP-01):**

```
1. Wrapper generates a CSR for the requested hostname
2. Wrapper contacts the ACME provider (default: Let's Encrypt)
3. ACME provider issues an HTTP-01 challenge token
4. Wrapper serves the challenge token at http://<host>/.well-known/acme-challenge/<token>
5. ACME provider verifies the challenge
6. ACME provider issues the certificate
7. Wrapper stores the certificate + private key encrypted on disk
8. Wrapper loads the certificate into the rustls ServerConfig
```

**Cluster CA flow:**

```
1. Wrapper generates a keypair locally
2. Wrapper creates a CSR containing the hostname(s)
3. Wrapper sends the CSR to its nearest council member over the inter-node mTLS channel
4. The council member validates the CSR against the app spec (the requesting node
   must be running an app with an ingress route for that hostname)
5. The council member signs the certificate with the Ingress CA private key
6. Wrapper receives the signed certificate
7. Wrapper stores the certificate + private key encrypted on disk
8. Wrapper loads the certificate into the rustls ServerConfig
```

**Certificate storage:**

Certificates are stored in `<data_dir>/ingress/certs/` with filenames derived from the hostname hash. Private keys are encrypted at rest using the node's Sesame-issued data encryption key. On startup, Wrapper loads all cached certificates and only requests new ones for routes whose certificates are missing or expired.

**Certificate renewal:**

- ACME certificates (90-day lifetime): renewed at 60 days (30 days before expiry)
- Cluster certificates (90-day lifetime): renewed at 60 days (30 days before expiry)
- Renewal runs as a background tokio task that checks certificate expiry every hour

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
    /// ACME (Let's Encrypt) — for public-facing services.
    Acme,
    /// Cluster Ingress CA — for internal services or air-gapped environments.
    Cluster,
    /// Auto: ACME if internet access, Cluster if air-gapped.
    Auto,
}

/// TLS configuration for a single hostname.
pub struct TlsConfig {
    /// Hostname this config applies to.
    pub hostname: String,
    /// TLS mode.
    pub mode: TlsMode,
    /// The current certificate chain (leaf + intermediates).
    pub cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// The private key.
    pub private_key: rustls::pki_types::PrivateKeyDer<'static>,
    /// When the leaf certificate expires.
    pub not_after: SystemTime,
    /// When renewal should be attempted (not_after - 30 days).
    pub renew_at: SystemTime,
    /// Serial number of the leaf certificate.
    pub serial: String,
}

/// ACME certificate state machine.
pub struct AcmeCertificate {
    /// Hostname being provisioned.
    pub hostname: String,
    /// Current state.
    pub state: AcmeState,
    /// ACME account URI.
    pub account_uri: String,
    /// ACME order URL (if provisioning).
    pub order_url: Option<String>,
    /// Challenge token (if awaiting validation).
    pub challenge_token: Option<String>,
    /// Challenge key authorisation (if awaiting validation).
    pub challenge_key_auth: Option<String>,
    /// Number of retry attempts for the current provisioning.
    pub retry_count: u32,
    /// Backoff deadline for retries.
    pub retry_after: Option<Instant>,
}

#[derive(Clone, Copy)]
pub enum AcmeState {
    /// No certificate, no pending order.
    NeedsCertificate,
    /// Order created, waiting for challenge to be set up.
    OrderCreated,
    /// Challenge response is being served on port 80, waiting for ACME validation.
    ChallengePending,
    /// Challenge validated, waiting for certificate issuance.
    CertificatePending,
    /// Certificate issued and loaded.
    Active,
    /// Renewal in progress (existing certificate still active).
    Renewing,
    /// Provisioning failed, will retry with backoff.
    Failed,
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
    /// Email address for ACME account registration.
    pub tls_acme_email: Option<String>,
    /// Default drain timeout for rolling deploys.
    pub drain_timeout: Duration,
    /// Global default rate limit (applied to routes without explicit config).
    pub default_rate_limit: Option<RateLimitConfig>,
    /// Active health probe configuration.
    pub health_probe: HealthProbeConfig,
    /// Minimum TLS version (default: TLS 1.2).
    pub min_tls_version: TlsVersion,
    /// ACME directory URL (default: Let's Encrypt production).
    pub acme_directory_url: String,
    /// Path to the certificate cache directory.
    pub cert_cache_dir: String,
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
    - Certificate selected from TlsConfig map by hostname
    - If no certificate matches SNI, connection is terminated with TLS alert
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

**Mode: `acme`**

Used for public-facing services where browsers and external clients need to trust the certificate. Wrapper provisions certificates from Let's Encrypt (or another ACME-compatible CA configured via `acme_directory_url`).

- **HTTP-01 challenge (default):** Wrapper serves the challenge token on port 80 at `/.well-known/acme-challenge/<token>`. This requires that external DNS for the hostname points to the cluster and that port 80 is reachable from the internet.
- **DNS-01 challenge (optional):** For wildcard certificates or environments where port 80 isn't reachable. Requires a DNS provider integration (configured via `[ingress.acme_dns]` in `node.toml`). Supported providers: Cloudflare, Route53, Google Cloud DNS.

**Mode: `cluster`**

Used for internal services, air-gapped environments, or services where clients trust the cluster's root CA. The certificate is signed by the Ingress CA (one of Sesame's three intermediate CAs). No external service is required.

Certificate signing follows the CSR model:

1. Wrapper generates a keypair and CSR locally.
2. CSR is sent to the nearest council member over the inter-node mTLS channel.
3. Council member validates that the requesting node runs an app with an ingress route for the requested hostname.
4. Council member signs the CSR with the Ingress CA private key (90-day lifetime).
5. Signed certificate is returned to Wrapper.

**Mode: `auto`**

Default when `tls` isn't explicitly specified. Resolves to `acme` if the cluster has internet access (determined during `relish init` or via `[cluster] air_gapped = false`). Resolves to `cluster` if `[cluster] air_gapped = true`.

### 5.3 ACME Certificate Provisioning

ACME provisioning is managed per hostname by the `AcmeCertificate` state machine:

```
NeedsCertificate ──► OrderCreated ──► ChallengePending ──► CertificatePending ──► Active
        ▲                                    │                       │
        │                                    ▼                       ▼
        └──────────────────────── Failed (retry with exponential backoff)
                                  1s → 2s → 4s → ... → max 1 hour
```

**Leader election for ACME:**

Because every node runs Wrapper, multiple nodes could attempt to provision a certificate for the same hostname simultaneously. To prevent this, ACME provisioning is leader-coordinated:

1. When Wrapper on any node detects that a route needs an ACME certificate, it sends a `CertificateRequest { hostname }` to the cluster leader via the reporting tree.
2. The leader selects one node (preferably the requesting node) to perform the ACME challenge.
3. Only the selected node interacts with the ACME provider.
4. Once the certificate is issued, the leader distributes it to all nodes via the reporting tree.
5. All nodes store the certificate locally and load it into their `rustls` config.

This ensures exactly one ACME order per hostname and avoids hitting Let's Encrypt rate limits.

**ACME account:**

A single ACME account is created during `relish init` (using the email from `tls_acme_email`) and stored encrypted in Raft state. All nodes use the same account credentials. The account key is an ECDSA P-256 key.

### 5.4 Certificate Renewal

A background tokio task runs every hour and iterates all loaded certificates:

```rust
async fn renewal_loop(certs: Arc<DashMap<String, TlsConfig>>) {
    loop {
        let now = SystemTime::now();
        for entry in certs.iter() {
            if now >= entry.renew_at {
                // Trigger renewal (same flow as initial provisioning)
                // Existing certificate continues serving traffic during renewal
                renew_certificate(entry.hostname.clone(), entry.mode).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
```

Renewal is non-disruptive: the old certificate continues serving traffic while the new one is being provisioned. Once issued, the new certificate is hot-swapped into the `rustls::ServerConfig` via a `rustls::server::ResolvesServerCert` implementation that reads from the `DashMap`. No connections are dropped.

If renewal fails, Wrapper retries with exponential backoff (1 hour, 2 hours, 4 hours, up to 24 hours). An alert fires in Mayo metrics and `relish wtf` reports the expiring certificate. At 7 days before expiry, the alert severity escalates to critical.

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
tls = "acme"
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
# Whether Wrapper is enabled on this node. Default: true.
enabled = true

# HTTP listener port. Used for ACME challenges and HTTPS redirects.
# Default: 80.
http_port = 80

# HTTPS listener port. Primary traffic port.
# Default: 443.
https_port = 443

# Email address for ACME account registration.
# Required if any route uses tls = "acme" or tls = "auto" on a non-air-gapped cluster.
tls_acme_email = "ops@example.com"

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

# ACME directory URL. Default: Let's Encrypt production.
# Use "https://acme-staging-v02.api.letsencrypt.org/directory" for testing.
acme_directory_url = "https://acme-v02.api.letsencrypt.org/directory"

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

# TLS mode. Default: "auto".
tls = "acme"

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
- **Missing ACME email:** If a route uses `tls = "acme"` and no `tls_acme_email` is configured, the deploy is rejected.
- **Invalid hostname:** Hostnames are validated against RFC 952 (alphanumeric, hyphens, dots, no wildcards in v1).
- **Path format:** Paths must start with `/` and not contain query strings or fragments.

---

## 7. Failure Modes

| Failure | Detection | Impact | Recovery |
|---|---|---|---|
| **ACME challenge failure** | ACME provider returns error or challenge times out after 5 minutes | New certificate not issued. If this is the initial certificate for a route, the route serves TLS errors (no valid cert). If renewing, the existing certificate continues serving. | Exponential backoff retry (1s → 1h max). Alert fires. Operator can debug with `relish ingress cert-status <hostname>`. Common causes: DNS not pointing to cluster, port 80 blocked by firewall, rate limit hit at ACME provider. |
| **Backend pool empty** | All backends removed from service map or all fail active health probes | Route returns 502 Bad Gateway for all requests. | Automatic: backends re-appear when health checks pass or new instances are scheduled. Wrapper re-adds them within seconds. |
| **Certificate expiry** | Renewal failed repeatedly and the certificate's `not_after` timestamp has passed | Clients receive TLS errors. Browsers show security warnings. API clients reject the connection. | Emergency: operator can manually provide a certificate via `relish ingress cert-install <hostname> --cert <file> --key <file>`. Root cause should be investigated (ACME provider blocked, Ingress CA unavailable, etc.). |
| **Slow draining** | In-flight connections exceed `drain_timeout` | Deploy step is delayed up to `drain_timeout`. After timeout, remaining connections are forcibly closed (TCP RST). | Increase `drain_timeout` if the app has legitimately long-running requests. For WebSocket apps, set a higher timeout or implement reconnection logic in the client. |
| **Council unreachable (cluster TLS)** | CSR request to council member times out | New `tls = "cluster"` certificates cannot be issued. Renewals fail. Existing certificates continue serving until they expire (90-day lifetime). | Council recovery. Wrapper retries CSR requests every 5 minutes. Alert fires when any certificate is within 30 days of expiry and renewal is failing. |
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

OCSP stapling: rustls supports OCSP stapling. Wrapper fetches and caches OCSP responses for ACME-issued certificates and staples them in the TLS handshake. For cluster-issued certificates, OCSP isn't applicable (no public responder).

### 8.2 ACME Account Security

- The ACME account private key is stored in Raft state, encrypted with the cluster's data encryption key.
- Only council members can decrypt the ACME account key. Worker nodes request ACME operations via the leader; they never hold the account key directly.
- ACME account deactivation: `relish ingress acme-deactivate` deactivates the ACME account and creates a new one. Use this if the account key is suspected to be compromised.

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
| Missing SNI handling | Connect without SNI. Verify connection is rejected (no default certificate). |
| Certificate hot-swap | Load a certificate, verify it is served. Replace with a new certificate. Verify new certificate is served without connection drops. |
| ACME HTTP-01 challenge response | Unit test: verify that `/.well-known/acme-challenge/<token>` returns the correct key authorisation. |
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

**What we do differently:** Traefik is a standalone process that must be deployed, configured, and updated separately. Wrapper is built into the Reliaburger binary. Traefik's ACME implementation is per-instance (requires shared storage for multi-instance setups); Wrapper coordinates ACME via the cluster leader.

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

**What we borrow:** Caddy's philosophy that TLS should be automatic and zero-configuration directly inspired Wrapper's `tls = "auto"` default. The idea that a proxy should handle certificate lifecycle without operator intervention is core to Wrapper's design.

**What we do differently:** Caddy is a standalone web server. Wrapper is embedded in the orchestrator binary. Caddy's ACME implementation is per-instance; Wrapper coordinates ACME cluster-wide via the leader to avoid duplicate orders and rate limit issues. Wrapper adds the `tls = "cluster"` mode (Ingress CA) for air-gapped environments, which Caddy doesn't support.

---

## 12. Libraries & Dependencies

All dependencies are Rust crates compiled into the single `reliaburger` binary.

| Crate | Version (min) | Purpose | Notes |
|---|---|---|---|
| [`hyper`](https://crates.io/crates/hyper) | 1.x | HTTP/1.1 and HTTP/2 server and client implementation | Used for both the listener (server) and the backend proxy connection (client). Provides streaming body support for efficient proxying. |
| [`rustls`](https://crates.io/crates/rustls) | 0.23.x | TLS implementation | Memory-safe TLS. No OpenSSL dependency. Supports TLS 1.2 and 1.3. SNI-based certificate selection via `ResolvesServerCert`. Session tickets for TLS 1.3 PSK resumption. |
| [`tokio`](https://crates.io/crates/tokio) | 1.x | Async runtime | Multi-threaded runtime with work-stealing scheduler. Provides `TcpListener`, `TcpStream`, timers, channels, and task spawning. Already used throughout the Bun agent. |
| [`instant-acme`](https://crates.io/crates/instant-acme) | 0.7.x | ACME protocol client | Async ACME client supporting HTTP-01 and DNS-01 challenges. Handles account creation, order management, challenge fulfillment, and certificate download. Alternative: `acme-lib` (synchronous API, less suitable for async context). |
| [`h2`](https://crates.io/crates/h2) | 0.4.x | HTTP/2 protocol implementation | Used by hyper internally. Listed explicitly because Wrapper must handle HTTP/2 connection-level concerns (flow control, stream multiplexing, GOAWAY frames during drain). |
| [`tungstenite`](https://crates.io/crates/tungstenite) | 0.24.x | WebSocket protocol implementation | Used for WebSocket upgrade detection and Close frame generation during connection draining. After the upgrade handshake, Wrapper uses raw TCP proxying (not tungstenite's frame parser) for performance. |
| [`tokio-tungstenite`](https://crates.io/crates/tokio-tungstenite) | 0.24.x | Async wrapper for tungstenite | Integrates tungstenite with tokio's async I/O model. Used during the upgrade handshake phase. |
| [`arc-swap`](https://crates.io/crates/arc-swap) | 1.x | Lock-free atomic Arc swapping | Used for the routing table swap during rebuilds. Readers (request handlers) never block. Writers (routing table rebuilder) swap in the new table atomically. |
| [`dashmap`](https://crates.io/crates/dashmap) | 6.x | Concurrent hash map | Used for the TLS certificate map (hostname → TlsConfig), rate limiter state (IP → TokenBucket), and connection tracker (ConnectionId → ConnectionState). |
| [`rcgen`](https://crates.io/crates/rcgen) | 0.13.x | X.509 certificate and CSR generation | Used to generate CSRs for both ACME and cluster CA certificate requests. Also used to generate the ACME account key. |
| [`webpki`](https://crates.io/crates/webpki) | 0.22.x | Certificate validation | Used to validate ACME-issued certificates after download (verify chain, check hostname, check expiry). |
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

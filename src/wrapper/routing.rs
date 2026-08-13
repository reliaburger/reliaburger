/// Routing table for the Wrapper ingress proxy.
///
/// Maps `(host, path)` pairs to backend pools. Rebuilt from the
/// `ServiceMap` whenever apps with ingress configuration change.
///
/// Ingress configs key on `(namespace, app_name)`, not the bare app
/// name: two apps of the same name in different namespaces must route
/// independently (D3/codex-M1). Their VIPs and service-map entries are
/// already namespace-distinct, so the routing table only needs the
/// namespaced key to look each one up.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::app::IngressSpec;
use crate::onion::service_id::ServiceId;
use crate::onion::service_map::ServiceMap;
use crate::onion::types::ServiceEntry;

use super::types::{LoadBalanceStrategy, RateLimitConfig, RouteInfo, TlsMode};

/// A single backend server.
#[derive(Debug, Clone)]
pub struct Backend {
    /// Instance ID (e.g. "web-0").
    pub instance_id: String,
    /// Real address to proxy to.
    pub addr: SocketAddr,
    /// Whether this backend is healthy per the cluster service map (passive
    /// health from the reporting tree).
    pub healthy: bool,
    /// Whether Wrapper's own active L7 probes consider this backend reachable
    /// from *this* node. Starts `true` on rebuild (trust the service map) and
    /// is flipped by the probe loop. A backend is only routable when both this
    /// and [`healthy`](Self::healthy) hold, so an instance that is healthy
    /// cluster-wide but unreachable locally is skipped.
    pub locally_healthy: bool,
}

/// A route for a specific path prefix within a host.
pub struct PathRoute {
    /// Host this route belongs to (lowercased, no port).
    pub host: String,
    /// Path prefix (e.g. "/api"). Empty string means "/".
    pub path_prefix: String,
    /// Namespace that owns this route.
    pub namespace: String,
    /// App that owns this route.
    pub app_name: String,
    /// Active backends (new requests go here).
    pub backends: Vec<Backend>,
    /// Load balancing strategy.
    pub lb_strategy: LoadBalanceStrategy,
    /// Round-robin counter.
    rr_counter: AtomicU64,
    /// Whether WebSocket upgrade is enabled.
    pub websocket: bool,
    /// Rate limiting config, if any.
    pub rate_limit: Option<RateLimitConfig>,
    /// TLS mode for this route. Anything other than [`TlsMode::Disabled`]
    /// means plain-HTTP requests get redirected to HTTPS (ING1).
    pub tls_mode: TlsMode,
}

/// A stable key identifying a route for per-route rate limiting.
///
/// Rate buckets key on `(host, path_prefix, client_ip)`, not the client IP
/// alone, so a client hitting `/api` can't spend the budget for `/` on the
/// same host (ING5).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RouteKey {
    /// Host the route belongs to.
    pub host: String,
    /// Path prefix the route matches.
    pub path_prefix: String,
}

impl PathRoute {
    /// Whether a backend is routable: healthy per the service map *and* passing
    /// Wrapper's own active probes.
    fn is_routable(b: &Backend) -> bool {
        b.healthy && b.locally_healthy
    }

    /// Select the next routable backend via round-robin.
    ///
    /// Returns `None` if no routable backends are available.
    pub fn select_backend(&self) -> Option<&Backend> {
        let healthy: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| Self::is_routable(b))
            .collect();
        if healthy.is_empty() {
            return None;
        }

        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % healthy.len();
        Some(healthy[idx])
    }

    /// Select up to `max` distinct routable backends for one request, in
    /// round-robin order.
    ///
    /// The first entry is the primary; the rest are failover candidates the
    /// proxy tries in order if the primary's connection fails. Returns fewer
    /// than `max` when the pool is smaller, and an empty vector when nothing is
    /// routable. Each backend appears at most once, so a failover always moves
    /// to a *different* instance rather than retrying the one that just failed.
    pub fn select_backends(&self, max: usize) -> Vec<(String, SocketAddr)> {
        let healthy: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| Self::is_routable(b))
            .collect();
        if healthy.is_empty() || max == 0 {
            return Vec::new();
        }
        let start = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize;
        let n = healthy.len();
        (0..n.min(max))
            .map(|offset| {
                let b = healthy[(start + offset) % n];
                (b.instance_id.clone(), b.addr)
            })
            .collect()
    }

    /// Number of routable backends (service-map healthy and locally probed OK).
    pub fn healthy_count(&self) -> usize {
        self.backends
            .iter()
            .filter(|b| Self::is_routable(b))
            .count()
    }

    /// Whether `path` falls under this route's prefix on a segment boundary.
    ///
    /// `/api` matches `/api` and `/api/v1` but not `/apievil` (ING5). The
    /// `/` route (prefix `""` or `"/"`) matches everything.
    fn matches_path(&self, path: &str) -> bool {
        let prefix = self.path_prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return true;
        }
        path == prefix || path.starts_with(&format!("{prefix}/"))
    }

    /// The rate-limit key for this route (host + path prefix).
    pub fn route_key(&self) -> RouteKey {
        RouteKey {
            host: self.host.clone(),
            path_prefix: self.path_prefix.clone(),
        }
    }
}

/// The routing table: hostname → sorted list of path routes.
///
/// Path routes are sorted by path length descending (longest prefix
/// match). The table is behind `Arc<RwLock<RoutingTable>>` — reads
/// are fast, writes happen only on deploy/health events.
pub struct RoutingTable {
    routes: HashMap<String, Vec<PathRoute>>,
}

impl RoutingTable {
    /// Create an empty routing table.
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    /// Rebuild the routing table from the ServiceMap and app ingress configs.
    ///
    /// `ingress_configs` maps `(namespace, app_name)` to their
    /// `IngressSpec`. Only apps with both a ServiceMap entry and an
    /// ingress config get routes.
    ///
    /// Returns an error listing any routes whose config was invalid
    /// (unsupported TLS mode, zero/overflow rate limit). Valid routes are
    /// still installed, so one bad app can't take the whole table down, but
    /// the caller learns which apps were rejected. The invalid route is
    /// never installed, so a divide-by-zero rate or a plaintext-served TLS
    /// route can't reach the request path (ING1/ING5).
    pub fn rebuild(
        &mut self,
        service_map: &ServiceMap,
        ingress_configs: &HashMap<(String, String), IngressSpec>,
    ) -> Result<(), RoutingError> {
        self.routes.clear();
        let mut rejected = Vec::new();

        for ((namespace, app_name), ingress) in ingress_configs {
            let service_id = ServiceId::new(namespace, app_name);
            let entry = match service_map.resolve(&service_id) {
                Some(e) => e,
                None => continue,
            };

            let host = ingress.host.to_lowercase();
            let route = match build_path_route(namespace, app_name, ingress, entry, &host) {
                Ok(route) => route,
                Err(reason) => {
                    rejected.push(RejectedRoute {
                        namespace: namespace.clone(),
                        app_name: app_name.clone(),
                        reason,
                    });
                    continue;
                }
            };

            self.routes.entry(host).or_default().push(route);
        }

        // Sort each host's routes longest-prefix first, breaking ties on the
        // path then app name so equal-length routes have a deterministic order
        // rather than depending on HashMap iteration (ING5).
        for routes in self.routes.values_mut() {
            routes.sort_by(|a, b| {
                b.path_prefix
                    .len()
                    .cmp(&a.path_prefix.len())
                    .then_with(|| a.path_prefix.cmp(&b.path_prefix))
                    .then_with(|| a.app_name.cmp(&b.app_name))
            });
        }

        if rejected.is_empty() {
            Ok(())
        } else {
            Err(RoutingError { rejected })
        }
    }

    /// Look up a route by host and path.
    ///
    /// Returns the first `PathRoute` whose prefix matches the request
    /// path on a segment boundary (longest prefix wins, since routes are
    /// sorted by length).
    pub fn lookup(&self, host: &str, path: &str) -> Option<&PathRoute> {
        let host_clean = normalise_host(host);
        let routes = self.routes.get(&host_clean)?;
        routes.iter().find(|r| r.matches_path(path))
    }

    /// List all routes as summary info.
    pub fn list_routes(&self) -> Vec<RouteInfo> {
        let mut result = Vec::new();
        for (host, routes) in &self.routes {
            for route in routes {
                result.push(RouteInfo {
                    host: host.clone(),
                    path: route.path_prefix.clone(),
                    app_name: route.app_name.clone(),
                    healthy_backends: route.healthy_count(),
                    total_backends: route.backends.len(),
                    websocket: route.websocket,
                });
            }
        }
        result
    }

    /// Number of hosts with routes.
    pub fn host_count(&self) -> usize {
        self.routes.len()
    }

    /// Whether `host` currently has any ingress route.
    ///
    /// Host is normalised the same way [`lookup`](Self::lookup) normalises it,
    /// so an SNI value matches regardless of case or trailing port. The TLS
    /// resolver uses this to mint a per-SNI certificate only for a configured
    /// ingress host — an arbitrary SNI can then neither drive CA signing nor
    /// grow the certificate cache.
    pub fn contains_host(&self, host: &str) -> bool {
        self.routes.contains_key(&normalise_host(host))
    }

    /// Total number of routes across all hosts.
    pub fn route_count(&self) -> usize {
        self.routes.values().map(|r| r.len()).sum()
    }

    /// Distinct `(instance_id, addr)` targets across every route.
    ///
    /// The active probe loop reads this to know which backends to probe.
    /// Deduplicated by instance ID, so a backend that appears on several routes
    /// is probed once.
    pub fn backend_targets(&self) -> Vec<(String, SocketAddr)> {
        let mut seen = std::collections::HashSet::new();
        let mut targets = Vec::new();
        for routes in self.routes.values() {
            for route in routes {
                for backend in &route.backends {
                    if seen.insert(backend.instance_id.clone()) {
                        targets.push((backend.instance_id.clone(), backend.addr));
                    }
                }
            }
        }
        targets
    }

    /// Set the active-probe health flag for every backend with `instance_id`.
    ///
    /// The probe loop calls this when an instance flips routable state. A
    /// locally-unhealthy backend is then skipped by [`PathRoute::select_backend`]
    /// even though the service map still lists it as healthy.
    pub fn set_local_health(&mut self, instance_id: &str, locally_healthy: bool) {
        for routes in self.routes.values_mut() {
            for route in routes {
                for backend in &mut route.backends {
                    if backend.instance_id == instance_id {
                        backend.locally_healthy = locally_healthy;
                    }
                }
            }
        }
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A route whose config the routing rebuild refused to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRoute {
    /// Namespace of the rejected app.
    pub namespace: String,
    /// App name of the rejected app.
    pub app_name: String,
    /// Why the route was rejected.
    pub reason: String,
}

/// One or more ingress configs were invalid during a routing rebuild.
#[derive(Debug, thiserror::Error)]
#[error("{} ingress route(s) rejected: {}", rejected.len(), summarise(rejected))]
pub struct RoutingError {
    /// The rejected routes and their reasons.
    pub rejected: Vec<RejectedRoute>,
}

fn summarise(rejected: &[RejectedRoute]) -> String {
    rejected
        .iter()
        .map(|r| format!("{}/{}: {}", r.namespace, r.app_name, r.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Normalise a Host header to the lookup key: lowercase, port stripped,
/// IPv6 brackets handled.
///
/// A bare `myapp.com:8080` becomes `myapp.com`. A bracketed IPv6 literal
/// `[::1]:8080` becomes `[::1]` — splitting naively on `:` would truncate
/// it to `[` and never match (ING5).
fn normalise_host(host: &str) -> String {
    let host = host.trim().to_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        // Bracketed IPv6: keep everything up to and including the closing
        // bracket, drop any `:port` after it.
        if let Some(end) = rest.find(']') {
            return format!("[{}]", &rest[..end]);
        }
        return host;
    }
    // Non-bracketed: a single trailing `:port` is a port; anything with more
    // than one colon is a bare IPv6 literal we leave as-is.
    match host.rsplit_once(':') {
        Some((left, _)) if !left.contains(':') => left.to_string(),
        _ => host,
    }
}

/// Build a PathRoute from an ingress config and service entry.
///
/// Returns `Err(reason)` if the TLS mode is unsupported or the rate limit
/// is out of range, so the caller can reject the route rather than install
/// something that would serve plaintext or divide by zero.
fn build_path_route(
    namespace: &str,
    app_name: &str,
    ingress: &IngressSpec,
    entry: &ServiceEntry,
    host: &str,
) -> Result<PathRoute, String> {
    let path_prefix = ingress.path.as_deref().unwrap_or("/").to_string();

    let backends: Vec<Backend> = entry
        .backends
        .iter()
        .map(|b| Backend {
            instance_id: b.instance_id.clone(),
            addr: SocketAddr::new(b.node_ip.into(), b.host_port),
            healthy: b.healthy,
            // Trust the service map on a fresh rebuild; the active probe loop
            // re-evaluates local reachability from here.
            locally_healthy: true,
        })
        .collect();

    let tls_mode = TlsMode::parse(ingress.tls.as_deref()).map_err(|e| e.to_string())?;

    let rate_limit = build_rate_limit(ingress.rate_limit_rps, ingress.rate_limit_burst)?;

    Ok(PathRoute {
        host: host.to_string(),
        path_prefix,
        namespace: namespace.to_string(),
        app_name: app_name.to_string(),
        backends,
        lb_strategy: LoadBalanceStrategy::default(),
        rr_counter: AtomicU64::new(0),
        websocket: ingress.websocket.unwrap_or(false),
        rate_limit,
        tls_mode,
    })
}

/// Validate and build a rate-limit config.
///
/// `rps` must be positive (a zero rate would divide by zero when computing
/// the retry-after), and the burst is derived as `rps * 2` when unset. Any
/// value that would overflow `u32` when doubled is rejected (ING5).
fn build_rate_limit(
    rps: Option<u32>,
    burst: Option<u32>,
) -> Result<Option<RateLimitConfig>, String> {
    let rps = match rps {
        None => return Ok(None),
        Some(rps) => rps,
    };
    if rps == 0 {
        return Err("rate_limit_rps must be greater than zero".to_string());
    }
    let burst = match burst {
        Some(0) => return Err("rate_limit_burst must be greater than zero".to_string()),
        Some(burst) => burst,
        None => rps
            .checked_mul(2)
            .ok_or_else(|| "rate_limit_rps too large to derive a default burst".to_string())?,
    };
    Ok(Some(RateLimitConfig { rps, burst }))
}

/// Configuration for Wrapper's active L7 health probes.
#[derive(Debug, Clone)]
pub struct HealthProbeConfig {
    /// How often to probe every backend.
    pub interval: Duration,
    /// Per-probe timeout.
    pub timeout: Duration,
    /// HTTP path to GET when probing.
    pub path: String,
    /// Consecutive failures before a backend is marked locally unhealthy.
    pub threshold_unhealthy: u32,
    /// Consecutive successes before a locally-unhealthy backend recovers.
    pub threshold_healthy: u32,
}

impl Default for HealthProbeConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(2),
            path: "/".to_string(),
            // Mirror the app health-check thresholds (`bun::health`): three
            // consecutive failures mark a backend down. Recovery takes two
            // successes rather than one, so a single lucky probe doesn't flap a
            // struggling backend straight back into rotation.
            threshold_unhealthy: 3,
            threshold_healthy: 2,
        }
    }
}

/// Per-instance consecutive probe counters and the current routable verdict.
#[derive(Debug, Clone)]
struct ProbeCounters {
    consecutive_healthy: u32,
    consecutive_unhealthy: u32,
    locally_healthy: bool,
}

impl ProbeCounters {
    fn new() -> Self {
        // A backend enters the tracker trusted (the service map already vouches
        // for it); probes only ever *demote* it from here.
        Self {
            consecutive_healthy: 0,
            consecutive_unhealthy: 0,
            locally_healthy: true,
        }
    }
}

/// Tracks consecutive active-probe results per backend and decides when a
/// backend flips between locally healthy and unhealthy.
///
/// This is the pure decision half of active probing — no I/O — so the
/// threshold logic is unit-testable without a network. It mirrors the
/// hysteresis in `bun::health`: N failures to go down, M successes to come
/// back, so a single flaky probe doesn't toggle a backend in or out.
#[derive(Debug)]
pub struct ProbeTracker {
    threshold_unhealthy: u32,
    threshold_healthy: u32,
    states: HashMap<String, ProbeCounters>,
}

impl ProbeTracker {
    /// Create a tracker with the given hysteresis thresholds.
    pub fn new(threshold_unhealthy: u32, threshold_healthy: u32) -> Self {
        Self {
            threshold_unhealthy: threshold_unhealthy.max(1),
            threshold_healthy: threshold_healthy.max(1),
            states: HashMap::new(),
        }
    }

    /// Record one probe result for `instance_id`.
    ///
    /// Returns `Some(true)` when the backend just recovered to locally healthy,
    /// `Some(false)` when it just went locally unhealthy, and `None` when the
    /// routable verdict is unchanged. The caller applies a returned verdict to
    /// the routing table.
    pub fn record(&mut self, instance_id: &str, success: bool) -> Option<bool> {
        let counters = self
            .states
            .entry(instance_id.to_string())
            .or_insert_with(ProbeCounters::new);

        if success {
            counters.consecutive_healthy = counters.consecutive_healthy.saturating_add(1);
            counters.consecutive_unhealthy = 0;
            if !counters.locally_healthy && counters.consecutive_healthy >= self.threshold_healthy {
                counters.locally_healthy = true;
                return Some(true);
            }
        } else {
            counters.consecutive_unhealthy = counters.consecutive_unhealthy.saturating_add(1);
            counters.consecutive_healthy = 0;
            if counters.locally_healthy
                && counters.consecutive_unhealthy >= self.threshold_unhealthy
            {
                counters.locally_healthy = false;
                return Some(false);
            }
        }
        None
    }

    /// Drop tracker state for instances no longer present in `live`.
    ///
    /// Called each sweep so a retired instance's counters don't leak; a new
    /// instance that reuses the ID starts fresh (trusted) rather than
    /// inheriting a stale unhealthy verdict.
    fn retain_only(&mut self, live: &std::collections::HashSet<String>) {
        self.states.retain(|id, _| live.contains(id));
    }
}

/// Probe one backend once. Success means it answered with a non-5xx status.
///
/// A connection failure, timeout, or 5xx all count as a failed probe: from
/// this node's point of view the backend can't serve a request right now.
async fn probe_backend_once(
    client: &reqwest::Client,
    addr: SocketAddr,
    path: &str,
    timeout: Duration,
) -> bool {
    let url = format!("http://{addr}{path}");
    match tokio::time::timeout(timeout, client.get(&url).send()).await {
        Ok(Ok(resp)) => !resp.status().is_server_error(),
        _ => false,
    }
}

/// Run Wrapper's active L7 health-probe loop until `shutdown` fires.
///
/// Every `config.interval` it probes each backend in the routing table and
/// feeds the result to a [`ProbeTracker`]. When an instance flips routable
/// state, it updates the routing table so [`PathRoute::select_backend`] starts
/// (or stops) skipping it. This catches backends that are healthy cluster-wide
/// but unreachable from this node — a signal the passive service map can't see.
pub async fn run_health_probes(
    routing_table: Arc<RwLock<RoutingTable>>,
    config: HealthProbeConfig,
    shutdown: CancellationToken,
) {
    let client = match reqwest::Client::builder().timeout(config.timeout).build() {
        Ok(c) => c,
        // Without a client we can't probe; passive service-map health still
        // governs routing, so simply stand down rather than crash the proxy.
        Err(_) => return,
    };
    let mut tracker = ProbeTracker::new(config.threshold_unhealthy, config.threshold_healthy);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(config.interval) => {}
        }

        let targets = routing_table.read().await.backend_targets();
        let live: std::collections::HashSet<String> =
            targets.iter().map(|(id, _)| id.clone()).collect();

        for (instance_id, addr) in &targets {
            let ok = probe_backend_once(&client, *addr, &config.path, config.timeout).await;
            if let Some(now_healthy) = tracker.record(instance_id, ok) {
                routing_table
                    .write()
                    .await
                    .set_local_health(instance_id, now_healthy);
            }
        }

        tracker.retain_only(&live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use crate::onion::types::BackendInstance;

    fn ns_key(namespace: &str, app: &str) -> (String, String) {
        (namespace.to_string(), app.to_string())
    }

    fn setup_map_and_configs() -> (ServiceMap, HashMap<(String, String), IngressSpec>) {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-1".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 4, 2),
                host_port: 30002,
                healthy: true,
            },
        )
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        (map, configs)
    }

    #[test]
    fn rebuild_populates_routes() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert_eq!(table.host_count(), 1);
        assert_eq!(table.route_count(), 1);
    }

    #[test]
    fn lookup_by_host_exact_match() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("myapp.com", "/").unwrap();
        assert_eq!(route.app_name, "web");
        assert_eq!(route.backends.len(), 2);
    }

    #[test]
    fn lookup_unknown_host_returns_none() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert!(table.lookup("other.com", "/").is_none());
    }

    #[test]
    fn contains_host_matches_configured_routes_only() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        // Configured host — case-insensitive and port-insensitive, like lookup.
        assert!(table.contains_host("myapp.com"));
        assert!(table.contains_host("MYAPP.COM"));
        assert!(table.contains_host("myapp.com:8443"));
        // Anything else is not a route.
        assert!(!table.contains_host("other.com"));
        assert!(!RoutingTable::new().contains_host("myapp.com"));
    }

    #[test]
    fn lookup_host_case_insensitive() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert!(table.lookup("MYAPP.COM", "/").is_some());
        assert!(table.lookup("MyApp.Com", "/").is_some());
    }

    #[test]
    fn lookup_strips_port_from_host() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert!(table.lookup("myapp.com:8080", "/").is_some());
    }

    #[test]
    fn longest_prefix_match() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: Some("/api".to_string()),
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: Some("/".to_string()),
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        // /api/v1 should match the /api route (longer prefix)
        let route = table.lookup("myapp.com", "/api/v1").unwrap();
        assert_eq!(route.app_name, "api");

        // /index.html should match the / route
        let route = table.lookup("myapp.com", "/index.html").unwrap();
        assert_eq!(route.app_name, "web");
    }

    #[test]
    fn round_robin_selects_backends() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("myapp.com", "/").unwrap();

        let b1 = route.select_backend().unwrap().instance_id.clone();
        let b2 = route.select_backend().unwrap().instance_id.clone();
        let b3 = route.select_backend().unwrap().instance_id.clone();
        let b4 = route.select_backend().unwrap().instance_id.clone();

        // Should alternate between web-0 and web-1
        assert_eq!(b1, b3);
        assert_eq!(b2, b4);
        assert_ne!(b1, b2);
    }

    #[test]
    fn round_robin_skips_unhealthy() {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-1".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 4, 2),
                host_port: 30002,
                healthy: false,
            },
        )
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("myapp.com", "/").unwrap();

        // Should always select web-0 (web-1 is unhealthy)
        for _ in 0..10 {
            let b = route.select_backend().unwrap();
            assert_eq!(b.instance_id, "web-0");
        }
    }

    #[test]
    fn no_healthy_backends_returns_none() {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: false,
            },
        )
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("myapp.com", "/").unwrap();
        assert!(route.select_backend().is_none());
    }

    #[test]
    fn list_routes_returns_all() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let routes = table.list_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].host, "myapp.com");
        assert_eq!(routes[0].app_name, "web");
        assert_eq!(routes[0].total_backends, 2);
        assert_eq!(routes[0].healthy_backends, 2);
    }

    #[test]
    fn same_name_apps_in_two_namespaces_route_independently() {
        // The D3/codex-M1 regression at the ingress layer: `api` in
        // `default` and `api` in `payments` both expose an ingress route,
        // on different hosts, and each must resolve to its own backends.
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();
        map.register_app("api", "payments", 3000, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", "api"),
            BackendInstance {
                instance_id: "api-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        map.add_backend(
            &ServiceId::new("payments", "api"),
            BackendInstance {
                instance_id: "api-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 9, 9),
                host_port: 40001,
                healthy: true,
            },
        )
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            IngressSpec {
                host: "default.example.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        configs.insert(
            ns_key("payments", "api"),
            IngressSpec {
                host: "payments.example.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        // Both routes exist and point at their own namespace's backend.
        let d = table.lookup("default.example.com", "/").unwrap();
        assert_eq!(d.namespace, "default");
        assert_eq!(d.backends[0].addr.port(), 30001);

        let p = table.lookup("payments.example.com", "/").unwrap();
        assert_eq!(p.namespace, "payments");
        assert_eq!(p.backends[0].addr.port(), 40001);
    }

    #[test]
    fn app_without_service_entry_skipped() {
        let map = ServiceMap::new(); // empty
        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert_eq!(table.host_count(), 0);
    }

    #[test]
    fn websocket_flag_from_ingress_spec() {
        let mut map = ServiceMap::new();
        map.register_app("ws", "default", 9090, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "ws"),
            IngressSpec {
                host: "ws.example.com".to_string(),
                path: None,
                tls: None,
                websocket: Some(true),
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("ws.example.com", "/").unwrap();
        assert!(route.websocket);
    }

    #[test]
    fn rate_limit_from_ingress_spec() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            IngressSpec {
                host: "api.example.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: Some(100),
                rate_limit_burst: Some(200),
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        let route = table.lookup("api.example.com", "/").unwrap();
        let rl = route.rate_limit.as_ref().unwrap();
        assert_eq!(rl.rps, 100);
        assert_eq!(rl.burst, 200);
    }

    fn spec_with_path(host: &str, path: &str) -> IngressSpec {
        IngressSpec {
            host: host.to_string(),
            path: Some(path.to_string()),
            tls: None,
            websocket: None,
            rate_limit_rps: None,
            rate_limit_burst: None,
        }
    }

    #[test]
    fn api_prefix_does_not_match_apievil() {
        // The classic prefix confusion: `/api` must not capture `/apievil`,
        // which belongs to the `/` route instead (ING5).
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            spec_with_path("myapp.com", "/api"),
        );
        configs.insert(ns_key("default", "web"), spec_with_path("myapp.com", "/"));

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        // /apievil must fall through to the / route, not the /api route.
        let route = table.lookup("myapp.com", "/apievil").unwrap();
        assert_eq!(route.app_name, "web");

        // /api and /api/v1 do match the /api route (segment boundary).
        assert_eq!(table.lookup("myapp.com", "/api").unwrap().app_name, "api");
        assert_eq!(
            table.lookup("myapp.com", "/api/v1").unwrap().app_name,
            "api"
        );
    }

    #[test]
    fn ipv6_host_with_port_routes_correctly() {
        // `[::1]:8080` must normalise to `[::1]`, not to `[` (ING5).
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(ns_key("default", "web"), spec_with_path("[::1]", "/"));

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        assert!(table.lookup("[::1]:8080", "/").is_some());
        assert!(table.lookup("[::1]", "/").is_some());
        // A bare (unbracketed) IPv6 literal without a port is left intact.
        assert_eq!(normalise_host("[2001:db8::1]:443"), "[2001:db8::1]");
        assert_eq!(normalise_host("myapp.com:8080"), "myapp.com");
        assert_eq!(normalise_host("MyApp.Com"), "myapp.com");
    }

    #[test]
    fn zero_rate_is_rejected_at_rebuild() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            IngressSpec {
                host: "api.example.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: Some(0),
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        let err = table.rebuild(&map, &configs).unwrap_err();
        assert_eq!(err.rejected.len(), 1);
        // The zero-rate route was not installed, so it can't divide by zero.
        assert!(table.lookup("api.example.com", "/").is_none());
    }

    #[test]
    fn overflow_burst_derivation_is_rejected() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "api"),
            IngressSpec {
                host: "api.example.com".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: Some(u32::MAX),
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        assert!(table.rebuild(&map, &configs).is_err());
    }

    #[test]
    fn unsupported_tls_mode_is_rejected() {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: Some("acme".to_string()),
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        // `acme` is not implemented, so the route is rejected rather than
        // served in plaintext (ING1).
        assert!(table.rebuild(&map, &configs).is_err());
        assert!(table.lookup("myapp.com", "/").is_none());
    }

    #[test]
    fn cluster_tls_mode_marks_the_route() {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            ns_key("default", "web"),
            IngressSpec {
                host: "myapp.com".to_string(),
                path: None,
                tls: Some("cluster".to_string()),
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );

        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();
        let route = table.lookup("myapp.com", "/").unwrap();
        assert_eq!(route.tls_mode, TlsMode::Cluster);
        assert!(route.tls_mode.requires_tls());
    }

    #[test]
    fn equal_length_routes_sort_deterministically() {
        // Two same-length prefixes on one host must resolve in a stable
        // order across rebuilds, not depend on HashMap iteration (ING5).
        let mut map = ServiceMap::new();
        map.register_app("aaa", "default", 3000, None).unwrap();
        map.register_app("bbb", "default", 3001, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(ns_key("default", "aaa"), spec_with_path("h.com", "/xyz"));
        configs.insert(ns_key("default", "bbb"), spec_with_path("h.com", "/abc"));

        // Rebuild several times; the /abc and /xyz routes must keep the same
        // relative order every time.
        for _ in 0..10 {
            let mut table = RoutingTable::new();
            table.rebuild(&map, &configs).unwrap();
            assert_eq!(table.lookup("h.com", "/abc/x").unwrap().app_name, "bbb");
            assert_eq!(table.lookup("h.com", "/xyz/x").unwrap().app_name, "aaa");
        }
    }

    #[test]
    fn locally_unhealthy_backend_is_skipped_by_selection() {
        // A backend that is healthy per the service map but has failed
        // Wrapper's active probes must not be selected: the router honours the
        // local verdict on top of the passive one.
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();

        // Both backends start routable.
        assert_eq!(table.lookup("myapp.com", "/").unwrap().healthy_count(), 2);

        // Probes mark web-1 locally unhealthy.
        table.set_local_health("web-1", false);

        let route = table.lookup("myapp.com", "/").unwrap();
        assert_eq!(route.healthy_count(), 1);
        for _ in 0..10 {
            assert_eq!(route.select_backend().unwrap().instance_id, "web-0");
        }

        // A recovery flips it back.
        table.set_local_health("web-1", true);
        assert_eq!(table.lookup("myapp.com", "/").unwrap().healthy_count(), 2);
    }

    #[test]
    fn select_backends_returns_distinct_failover_candidates() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();
        let route = table.lookup("myapp.com", "/").unwrap();

        // Two healthy backends, so a request gets a primary plus one distinct
        // failover candidate — never the same instance twice.
        let picks = route.select_backends(3);
        assert_eq!(picks.len(), 2);
        assert_ne!(picks[0].0, picks[1].0);

        // Zero max yields nothing.
        assert!(route.select_backends(0).is_empty());
    }

    #[test]
    fn probe_tracker_marks_unhealthy_after_three_failures() {
        // Mirrors bun::health: the third consecutive failure flips the verdict,
        // and not before.
        let mut tracker = ProbeTracker::new(3, 2);
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", false), Some(false));
    }

    #[test]
    fn probe_tracker_recovers_after_two_successes() {
        let mut tracker = ProbeTracker::new(3, 2);
        // Drive it unhealthy first.
        tracker.record("web-0", false);
        tracker.record("web-0", false);
        assert_eq!(tracker.record("web-0", false), Some(false));

        // One success is not enough to recover; the second flips it back.
        assert_eq!(tracker.record("web-0", true), None);
        assert_eq!(tracker.record("web-0", true), Some(true));
    }

    #[test]
    fn probe_tracker_resets_streak_on_alternating_results() {
        // A single success in the middle of a failure streak resets the count,
        // so alternating results never trip the threshold.
        let mut tracker = ProbeTracker::new(3, 2);
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", true), None); // resets unhealthy streak
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", false), None);
        assert_eq!(tracker.record("web-0", false), Some(false));
    }
}

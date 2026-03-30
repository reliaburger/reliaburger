/// Routing table for the Wrapper ingress proxy.
///
/// Maps `(host, path)` pairs to backend pools. Rebuilt from the
/// `ServiceMap` whenever apps with ingress configuration change.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::app::IngressSpec;
use crate::onion::service_map::ServiceMap;
use crate::onion::types::ServiceEntry;

use super::types::{LoadBalanceStrategy, RateLimitConfig, RouteInfo};

/// A single backend server.
#[derive(Debug, Clone)]
pub struct Backend {
    /// Instance ID (e.g. "web-0").
    pub instance_id: String,
    /// Real address to proxy to.
    pub addr: SocketAddr,
    /// Whether this backend is healthy.
    pub healthy: bool,
}

/// A route for a specific path prefix within a host.
pub struct PathRoute {
    /// Path prefix (e.g. "/api"). Empty string means "/".
    pub path_prefix: String,
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
}

impl PathRoute {
    /// Select the next healthy backend via round-robin.
    ///
    /// Returns `None` if no healthy backends are available.
    pub fn select_backend(&self) -> Option<&Backend> {
        let healthy: Vec<&Backend> = self.backends.iter().filter(|b| b.healthy).collect();
        if healthy.is_empty() {
            return None;
        }

        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % healthy.len();
        Some(healthy[idx])
    }

    /// Number of healthy backends.
    pub fn healthy_count(&self) -> usize {
        self.backends.iter().filter(|b| b.healthy).count()
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
    /// `ingress_configs` maps app names to their `IngressSpec`. Only
    /// apps with both a ServiceMap entry and an ingress config get routes.
    pub fn rebuild(
        &mut self,
        service_map: &ServiceMap,
        ingress_configs: &HashMap<String, IngressSpec>,
    ) {
        self.routes.clear();

        for (app_name, ingress) in ingress_configs {
            let entry = match service_map.resolve(app_name) {
                Some(e) => e,
                None => continue,
            };

            let route = build_path_route(app_name, ingress, entry);
            let host = ingress.host.to_lowercase();

            self.routes.entry(host).or_default().push(route);
        }

        // Sort each host's routes by path length descending (longest prefix match)
        for routes in self.routes.values_mut() {
            routes.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));
        }
    }

    /// Look up a route by host and path.
    ///
    /// Returns the first `PathRoute` whose prefix matches the request
    /// path (longest prefix wins, since routes are sorted by length).
    pub fn lookup(&self, host: &str, path: &str) -> Option<&PathRoute> {
        let host_lower = host.to_lowercase();
        // Strip port from host header (e.g. "myapp.com:8080" → "myapp.com")
        let host_clean = host_lower.split(':').next().unwrap_or(&host_lower);

        let routes = self.routes.get(host_clean)?;
        routes.iter().find(|r| path.starts_with(&r.path_prefix))
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

    /// Total number of routes across all hosts.
    pub fn route_count(&self) -> usize {
        self.routes.values().map(|r| r.len()).sum()
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a PathRoute from an ingress config and service entry.
fn build_path_route(app_name: &str, ingress: &IngressSpec, entry: &ServiceEntry) -> PathRoute {
    let path_prefix = ingress.path.as_deref().unwrap_or("/").to_string();

    let backends: Vec<Backend> = entry
        .backends
        .iter()
        .map(|b| Backend {
            instance_id: b.instance_id.clone(),
            addr: SocketAddr::new(b.node_ip.into(), b.host_port),
            healthy: b.healthy,
        })
        .collect();

    let rate_limit = match (ingress.rate_limit_rps, ingress.rate_limit_burst) {
        (Some(rps), burst) => Some(RateLimitConfig {
            rps,
            burst: burst.unwrap_or(rps * 2),
        }),
        _ => None,
    };

    PathRoute {
        path_prefix,
        app_name: app_name.to_string(),
        backends,
        lb_strategy: LoadBalanceStrategy::default(),
        rr_counter: AtomicU64::new(0),
        websocket: ingress.websocket.unwrap_or(false),
        rate_limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use crate::onion::types::BackendInstance;

    fn setup_map_and_configs() -> (ServiceMap, HashMap<String, IngressSpec>) {
        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();
        map.add_backend(
            "web",
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        map.add_backend(
            "web",
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
            "web".to_string(),
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
        table.rebuild(&map, &configs);

        assert_eq!(table.host_count(), 1);
        assert_eq!(table.route_count(), 1);
    }

    #[test]
    fn lookup_by_host_exact_match() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs);

        let route = table.lookup("myapp.com", "/").unwrap();
        assert_eq!(route.app_name, "web");
        assert_eq!(route.backends.len(), 2);
    }

    #[test]
    fn lookup_unknown_host_returns_none() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs);

        assert!(table.lookup("other.com", "/").is_none());
    }

    #[test]
    fn lookup_host_case_insensitive() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs);

        assert!(table.lookup("MYAPP.COM", "/").is_some());
        assert!(table.lookup("MyApp.Com", "/").is_some());
    }

    #[test]
    fn lookup_strips_port_from_host() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs);

        assert!(table.lookup("myapp.com:8080", "/").is_some());
    }

    #[test]
    fn longest_prefix_match() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();
        map.register_app("web", "default", 8080, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            "api".to_string(),
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
            "web".to_string(),
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
        table.rebuild(&map, &configs);

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
        table.rebuild(&map, &configs);

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
            "web",
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        map.add_backend(
            "web",
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
            "web".to_string(),
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
        table.rebuild(&map, &configs);

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
            "web",
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
            "web".to_string(),
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
        table.rebuild(&map, &configs);

        let route = table.lookup("myapp.com", "/").unwrap();
        assert!(route.select_backend().is_none());
    }

    #[test]
    fn list_routes_returns_all() {
        let (map, configs) = setup_map_and_configs();
        let mut table = RoutingTable::new();
        table.rebuild(&map, &configs);

        let routes = table.list_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].host, "myapp.com");
        assert_eq!(routes[0].app_name, "web");
        assert_eq!(routes[0].total_backends, 2);
        assert_eq!(routes[0].healthy_backends, 2);
    }

    #[test]
    fn app_without_service_entry_skipped() {
        let map = ServiceMap::new(); // empty
        let mut configs = HashMap::new();
        configs.insert(
            "web".to_string(),
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
        table.rebuild(&map, &configs);

        assert_eq!(table.host_count(), 0);
    }

    #[test]
    fn websocket_flag_from_ingress_spec() {
        let mut map = ServiceMap::new();
        map.register_app("ws", "default", 9090, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            "ws".to_string(),
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
        table.rebuild(&map, &configs);

        let route = table.lookup("ws.example.com", "/").unwrap();
        assert!(route.websocket);
    }

    #[test]
    fn rate_limit_from_ingress_spec() {
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();

        let mut configs = HashMap::new();
        configs.insert(
            "api".to_string(),
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
        table.rebuild(&map, &configs);

        let route = table.lookup("api.example.com", "/").unwrap();
        let rl = route.rate_limit.as_ref().unwrap();
        assert_eq!(rl.rps, 100);
        assert_eq!(rl.burst, 200);
    }
}

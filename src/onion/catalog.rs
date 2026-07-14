/// Cluster-wide replicated service endpoint catalogue.
///
/// The `ServiceMap` (`service_map.rs`) is node-local: it only knows about
/// backends running on *this* node. That's fine for a single node, but in
/// a cluster a container on node A must be able to reach a service whose
/// backends live on node B. The `EndpointCatalog` is the missing piece —
/// the leader builds it from every node's health reports and publishes it
/// into the replicated `DesiredState`, so every node sees every namespace's
/// healthy backends and can resolve them.
///
/// It's deliberately self-describing and append-only-friendly: a
/// `BTreeMap` keyed by the namespace-qualified service id, each value a
/// small owned record. That makes the JSON snapshot deterministic and lets
/// a pre-theme snapshot (which has no catalogue at all) load cleanly under
/// `#[serde(default)]`.
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::service_id::ServiceId;
use super::vip::VirtualIP;

/// One backend endpoint of a service, somewhere in the cluster.
///
/// Carries the real address a connection should land on plus the health
/// flag, so a resolving node can pick a live backend without a second
/// lookup. `node_id` is informational (which node runs it) and lets the
/// leader rebuild the catalogue idempotently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogBackend {
    /// Name of the node running this backend.
    pub node_id: String,
    /// Real node IP the backend listens on.
    pub node_ip: Ipv4Addr,
    /// Dynamically allocated host port.
    pub host_port: u16,
    /// Whether the backend is currently healthy.
    pub healthy: bool,
}

/// A service's cluster-wide entry: its VIP, declared port, and every
/// backend across the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogService {
    /// The service's virtual IP. Deterministic from the qualified id, but
    /// stored so the allocation the leader chose (which may have probed off
    /// the natural hash on a collision) is authoritative cluster-wide.
    pub vip: VirtualIP,
    /// Declared container port.
    pub port: u16,
    /// Every backend of this service, on any node.
    pub backends: Vec<CatalogBackend>,
}

/// The replicated catalogue: qualified service id → its cluster entry.
///
/// Keyed by `ServiceId::qualified()` (`{namespace}__{name}`) in a
/// `BTreeMap` for deterministic JSON. Cluster-wide VIP allocation is done
/// here so the same service resolves to the same VIP on every node, and a
/// hash collision between two *different* services is resolved once,
/// centrally, rather than raced per node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCatalog {
    /// Services keyed by their qualified id.
    pub services: BTreeMap<String, CatalogService>,
}

impl EndpointCatalog {
    /// An empty catalogue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a service by its namespace-qualified identity.
    pub fn resolve(&self, id: &ServiceId) -> Option<&CatalogService> {
        self.services.get(&id.qualified())
    }

    /// Number of services in the catalogue.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    /// Whether the catalogue is empty.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Rebuild the catalogue from a flat list of `(ServiceId, port,
    /// backends)` gathered from node reports.
    ///
    /// VIPs are allocated cluster-wide and deterministically: each service
    /// hashes its qualified id, and on the rare collision with a VIP
    /// already assigned to a *different* service the allocation probes
    /// deterministic successors (`{qualified}#1`, `#2`, …). Because both
    /// the input order (a `BTreeMap` sorts by qualified id) and the probe
    /// sequence are deterministic, every node that rebuilds from the same
    /// reports arrives at the same VIP assignment — but only the leader
    /// rebuilds and replicates, so followers never disagree.
    pub fn rebuild(
        services: impl IntoIterator<Item = (ServiceId, u16, Vec<CatalogBackend>)>,
    ) -> Self {
        // Collect into a BTreeMap first for a deterministic allocation order.
        let mut inputs: BTreeMap<String, (ServiceId, u16, Vec<CatalogBackend>)> = BTreeMap::new();
        for (id, port, backends) in services {
            inputs.insert(id.qualified(), (id, port, backends));
        }

        let mut allocated: std::collections::HashSet<VirtualIP> = std::collections::HashSet::new();
        let mut catalogue = EndpointCatalog::new();
        for (qualified, (id, port, backends)) in inputs {
            let vip = allocate_vip(&id, &allocated);
            allocated.insert(vip);
            catalogue.services.insert(
                qualified,
                CatalogService {
                    vip,
                    port,
                    backends,
                },
            );
        }
        catalogue
    }
}

/// Usable VIP slots in `127.128.0.0/16` (65,534: excludes .0.0 and .255.255).
const VIP_SPACE: u32 = 65_534;

/// Allocate a cluster-unique VIP for `id`, probing deterministic
/// successors on a collision with an already-allocated VIP.
fn allocate_vip(id: &ServiceId, allocated: &std::collections::HashSet<VirtualIP>) -> VirtualIP {
    let base = id.qualified();
    let vip = VirtualIP::from_qualified(&base);
    if !allocated.contains(&vip) {
        return vip;
    }
    for attempt in 1..VIP_SPACE {
        let candidate = VirtualIP::from_qualified(&format!("{base}#{attempt}"));
        if !allocated.contains(&candidate) {
            return candidate;
        }
    }
    // The whole /16 is occupied — 65k services on a node is far past any
    // real limit. Fall back to the natural VIP; a shared VIP degrades
    // routing for two services but never panics.
    vip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(node: &str, ip: [u8; 4], port: u16, healthy: bool) -> CatalogBackend {
        CatalogBackend {
            node_id: node.to_string(),
            node_ip: Ipv4Addr::from(ip),
            host_port: port,
            healthy,
        }
    }

    #[test]
    fn rebuild_assigns_deterministic_namespaced_vips() {
        let cat = EndpointCatalog::rebuild([
            (
                ServiceId::new("default", "api"),
                3000,
                vec![backend("node-a", [10, 0, 0, 1], 30001, true)],
            ),
            (
                ServiceId::new("payments", "api"),
                3000,
                vec![backend("node-b", [10, 0, 0, 2], 30002, true)],
            ),
        ]);

        let d = cat.resolve(&ServiceId::new("default", "api")).unwrap();
        let p = cat.resolve(&ServiceId::new("payments", "api")).unwrap();
        // Same name, two namespaces, distinct VIPs — the D3 fix at cluster scope.
        assert_ne!(d.vip, p.vip);
        assert_eq!(
            d.vip,
            VirtualIP::from_service_id(&ServiceId::new("default", "api"))
        );
        assert_eq!(
            p.vip,
            VirtualIP::from_service_id(&ServiceId::new("payments", "api"))
        );
    }

    #[test]
    fn rebuild_is_order_independent() {
        let a = EndpointCatalog::rebuild([
            (ServiceId::new("default", "web"), 80, vec![]),
            (ServiceId::new("default", "api"), 3000, vec![]),
        ]);
        let b = EndpointCatalog::rebuild([
            (ServiceId::new("default", "api"), 3000, vec![]),
            (ServiceId::new("default", "web"), 80, vec![]),
        ]);
        assert_eq!(a, b, "catalogue must not depend on input order");
    }

    #[test]
    fn colliding_services_get_distinct_vips() {
        // Two services whose natural VIPs collide must not share one: the
        // second probes to a successor. We can't easily force a natural
        // collision, so assert the invariant on a rebuild: every VIP unique.
        let cat = EndpointCatalog::rebuild(
            (0..500).map(|i| (ServiceId::new("default", format!("svc-{i}")), 8080, vec![])),
        );
        let unique: std::collections::HashSet<_> = cat.services.values().map(|s| s.vip).collect();
        assert_eq!(
            unique.len(),
            cat.services.len(),
            "two services shared a VIP"
        );
    }

    #[test]
    fn resolve_returns_backends() {
        let cat = EndpointCatalog::rebuild([(
            ServiceId::new("default", "redis"),
            6379,
            vec![
                backend("node-a", [10, 0, 0, 1], 30001, true),
                backend("node-b", [10, 0, 0, 2], 30002, false),
            ],
        )]);
        let svc = cat.resolve(&ServiceId::new("default", "redis")).unwrap();
        assert_eq!(svc.port, 6379);
        assert_eq!(svc.backends.len(), 2);
        assert!(svc.backends[0].healthy);
        assert!(!svc.backends[1].healthy);
    }

    #[test]
    fn empty_catalogue_resolves_nothing() {
        let cat = EndpointCatalog::new();
        assert!(cat.is_empty());
        assert_eq!(cat.len(), 0);
        assert!(cat.resolve(&ServiceId::new("default", "ghost")).is_none());
    }

    #[test]
    fn catalogue_json_round_trips() {
        let cat = EndpointCatalog::rebuild([(
            ServiceId::new("payments", "api"),
            3000,
            vec![backend("node-a", [10, 0, 0, 1], 30001, true)],
        )]);
        let json = serde_json::to_string(&cat).unwrap();
        let back: EndpointCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(cat, back);
    }
}

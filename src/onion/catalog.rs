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

/// Maximum retained discovery consumers; reaching the bound never evicts one.
pub const MAX_ENDPOINT_CONSUMERS: usize = 65_536;

/// One backend endpoint of a service, somewhere in the cluster.
///
/// Carries the real address a connection should land on plus the health
/// flag, so a resolving node can pick a live backend without a second
/// lookup. `node_id` is informational (which node runs it) and lets the
/// leader rebuild the catalogue idempotently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogBackend {
    /// Original execution identity, when durable runtime evidence is available.
    #[serde(default)]
    pub execution: Option<crate::grill::RuntimeExecution>,
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

    /// Refresh active services while preserving their previously allocated VIPs.
    /// Invalid prior allocations and exhausted address space refuse the update.
    /// Callers retain retiring services until their separate withdrawal proof completes.
    pub fn reconcile(
        &self,
        services: impl IntoIterator<Item = (ServiceId, u16, Vec<CatalogBackend>)>,
    ) -> Result<Self, super::types::OnionError> {
        use super::types::OnionError;
        let mut original_vips = std::collections::HashSet::new();
        for (qualified, service) in &self.services {
            let valid_id = ServiceId::parse(qualified).is_some_and(|id| {
                crate::config::valid_workload_label(&id.namespace)
                    && crate::config::valid_workload_label(&id.name)
            });
            if !valid_id
                || service.port == 0
                || !(0x7f80_0001..=0x7f80_fffe).contains(&u32::from(service.vip.0))
                || !original_vips.insert(service.vip)
            {
                return Err(OnionError::InvalidSnapshot {
                    service: qualified.clone(),
                    reason: "invalid or conflicting catalogue allocation",
                });
            }
        }
        let mut inputs = BTreeMap::new();
        for (id, port, backends) in services {
            let qualified = id.qualified();
            if !crate::config::valid_workload_label(&id.namespace)
                || !crate::config::valid_workload_label(&id.name)
                || port == 0
            {
                return Err(OnionError::InvalidSnapshot {
                    service: qualified,
                    reason: "invalid catalogue service identity or port",
                });
            }
            if inputs
                .insert(qualified.clone(), (id, port, backends))
                .is_some()
            {
                return Err(OnionError::AlreadyRegistered { name: qualified });
            }
            if inputs.len() > VIP_SPACE as usize {
                return Err(OnionError::VipSpaceExhausted { name: qualified });
            }
        }
        // Reserve every retained allocation before a newcomer can claim its hash.
        let mut allocated: std::collections::HashSet<_> = self
            .services
            .iter()
            .filter(|(qualified, _)| inputs.contains_key(*qualified))
            .map(|(_, service)| service.vip)
            .collect();
        let mut catalogue = Self::new();
        for (qualified, (id, port, backends)) in inputs {
            let vip = match self.services.get(&qualified) {
                Some(original) => original.vip,
                None => allocate_vip(&id, &allocated)?,
            };
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
        Ok(catalogue)
    }

    /// Allocate a fresh deterministic catalogue, refusing invalid input or exhaustion.
    /// Existing catalogues must use reconcile to preserve original allocations.
    pub fn rebuild(
        services: impl IntoIterator<Item = (ServiceId, u16, Vec<CatalogBackend>)>,
    ) -> Result<Self, super::types::OnionError> {
        Self::new().reconcile(services)
    }
}

/// Usable VIP slots in `127.128.0.0/16` (65,534: excludes .0.0 and .255.255).
const VIP_SPACE: u32 = 65_534;

/// Allocate a cluster-unique VIP for `id`, probing deterministic
/// successors on a collision with an already-allocated VIP.
fn allocate_vip(
    id: &ServiceId,
    allocated: &std::collections::HashSet<VirtualIP>,
) -> Result<VirtualIP, super::types::OnionError> {
    let base = id.qualified();
    let vip = VirtualIP::from_qualified(&base);
    if !allocated.contains(&vip) {
        return Ok(vip);
    }
    for attempt in 1..VIP_SPACE {
        let candidate = VirtualIP::from_qualified(&format!("{base}#{attempt}"));
        if !allocated.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(super::types::OnionError::VipSpaceExhausted { name: base })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(node: &str, ip: [u8; 4], port: u16, healthy: bool) -> CatalogBackend {
        CatalogBackend {
            execution: None,
            node_id: node.to_string(),
            node_ip: Ipv4Addr::from(ip),
            host_port: port,
            healthy,
        }
    }

    fn colliding_ids() -> (ServiceId, ServiceId) {
        let mut seen = std::collections::HashMap::new();
        let mut pair = (0..65_535)
            .find_map(|index| {
                let id = ServiceId::new("default", format!("collision-{index}"));
                seen.insert(VirtualIP::from_service_id(&id), id.clone())
                    .map(|first| [first, id])
            })
            .unwrap();
        pair.sort_by_key(ServiceId::qualified);
        (pair[0].clone(), pair[1].clone())
    }

    #[test]
    fn refresh_preserves_an_active_vip_when_a_colliding_service_arrives() {
        let (first, second) = colliding_ids();
        let previous = EndpointCatalog::rebuild([(second.clone(), 8080, vec![])]).unwrap();
        let vip = previous.resolve(&second).unwrap().vip;
        let updated = previous
            .reconcile([
                (first.clone(), 8080, vec![]),
                (second.clone(), 8080, vec![]),
            ])
            .unwrap();
        assert_eq!(
            updated.resolve(&second).unwrap().vip,
            vip,
            "an existing service moved to a different VIP"
        );
        assert_ne!(updated.resolve(&first).unwrap().vip, vip);
    }

    #[test]
    fn refresh_preserves_a_collision_resolved_vip_when_the_other_service_departs() {
        let (first, second) = colliding_ids();
        let previous =
            EndpointCatalog::rebuild([(first, 8080, vec![]), (second.clone(), 8080, vec![])])
                .unwrap();
        let vip = previous.resolve(&second).unwrap().vip;
        let updated = previous
            .reconcile([(second.clone(), 8080, vec![])])
            .unwrap();
        assert_eq!(
            updated.resolve(&second).unwrap().vip,
            vip,
            "a retained allocation was replaced by its natural hash"
        );
    }

    #[test]
    fn refresh_refuses_conflicting_original_allocations() {
        let first = ServiceId::new("default", "first");
        let second = ServiceId::new("default", "second");
        let mut previous = EndpointCatalog::rebuild([
            (first.clone(), 8080, vec![]),
            (second.clone(), 8080, vec![]),
        ])
        .unwrap();
        let vip = previous.resolve(&first).unwrap().vip;
        previous.services.get_mut(&second.qualified()).unwrap().vip = vip;
        assert!(
            previous
                .reconcile([(first, 8080, vec![]), (second, 8080, vec![])])
                .is_err()
        );
    }

    #[test]
    fn refresh_refuses_more_services_than_the_vip_space() {
        assert!(
            EndpointCatalog::new()
                .reconcile((0..=VIP_SPACE).map(|i| (
                    ServiceId::new("default", format!("service-{i}")),
                    8080,
                    vec![]
                )))
                .is_err(),
            "exhaustion silently shared a VIP"
        );
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
        ])
        .unwrap();

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
        ])
        .unwrap();
        let b = EndpointCatalog::rebuild([
            (ServiceId::new("default", "api"), 3000, vec![]),
            (ServiceId::new("default", "web"), 80, vec![]),
        ])
        .unwrap();
        assert_eq!(a, b, "catalogue must not depend on input order");
    }

    #[test]
    fn colliding_services_get_distinct_vips() {
        // Two services whose natural VIPs collide must not share one: the
        // second probes to a successor. We can't easily force a natural
        // collision, so assert the invariant on a rebuild: every VIP unique.
        let cat = EndpointCatalog::rebuild(
            (0..500).map(|i| (ServiceId::new("default", format!("svc-{i}")), 8080, vec![])),
        )
        .unwrap();
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
        )])
        .unwrap();
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
        )])
        .unwrap();
        let json = serde_json::to_string(&cat).unwrap();
        let back: EndpointCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(cat, back);
    }
}

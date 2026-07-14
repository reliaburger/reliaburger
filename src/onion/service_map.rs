/// Userspace service map for Onion.
///
/// Maintains the mapping from namespace-qualified service identities to
/// virtual IPs and backend lists. This is the source of truth that gets
/// synced to BPF maps on Linux. On all platforms, it powers
/// `relish resolve`.
///
/// The map keys on [`ServiceId`] (namespace + name), not the bare app
/// name: keying on the bare name let `default/api` and `payments/api`
/// collide (D3/codex-M1). Internally we store the canonical qualified
/// string (`{namespace}__{name}`) as the `HashMap` key so lookups stay a
/// plain string hash.
use std::collections::{HashMap, HashSet};

use super::service_id::ServiceId;
use super::types::{BackendInstance, MAX_BACKENDS, OnionError, ServiceEntry};
use super::vip::{VirtualIP, name_to_id};

/// The service map: qualified service identities to their service entries.
///
/// All mutations go through this struct's methods, which enforce
/// invariants (max backends, unique instance IDs, VIP uniqueness, etc.).
///
/// `Clone` exists so the agent can publish read-only snapshots over a
/// `watch` channel; the DNS responder resolves from those snapshots
/// without contending with the agent's event loop.
#[derive(Clone)]
pub struct ServiceMap {
    /// Keyed by `ServiceId::qualified()`.
    entries: HashMap<String, ServiceEntry>,
    /// Every VIP currently in use, so a fresh registration can detect a
    /// hash collision and probe for a free successor rather than
    /// silently sharing a VIP with an unrelated service.
    allocated_vips: HashSet<VirtualIP>,
}

impl ServiceMap {
    /// Create an empty service map.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            allocated_vips: HashSet::new(),
        }
    }

    /// Register a new service in the map.
    ///
    /// Computes the VIP deterministically from the namespace-qualified
    /// identity, so two apps of the same name in different namespaces
    /// get distinct VIPs. Starts with an empty backend list — backends
    /// are added as instances reach the Running state.
    pub fn register(
        &mut self,
        id: &ServiceId,
        port: u16,
        firewall_allow_from: Option<Vec<String>>,
    ) -> Result<VirtualIP, OnionError> {
        let key = id.qualified();
        if self.entries.contains_key(&key) {
            return Err(OnionError::AlreadyRegistered { name: key });
        }

        let vip = self.allocate_vip(id)?;

        let entry = ServiceEntry {
            app_name: id.name.clone(),
            namespace: id.namespace.clone(),
            namespace_id: name_to_id(&id.namespace),
            app_id: name_to_id(&id.name),
            vip,
            port,
            backends: Vec::new(),
            firewall_allow_from,
        };

        self.allocated_vips.insert(vip);
        self.entries.insert(key, entry);
        Ok(vip)
    }

    /// Register by namespace and name. Convenience for call sites that
    /// hold the two strings separately.
    pub fn register_app(
        &mut self,
        app_name: &str,
        namespace: &str,
        port: u16,
        firewall_allow_from: Option<Vec<String>>,
    ) -> Result<VirtualIP, OnionError> {
        self.register(
            &ServiceId::new(namespace, app_name),
            port,
            firewall_allow_from,
        )
    }

    /// Allocate a free VIP for `id`.
    ///
    /// The VIP is deterministic from the qualified identity. On the rare
    /// hash collision with an already-allocated VIP, we probe successor
    /// identities (`{qualified}#1`, `#2`, …) deterministically until a
    /// free slot is found, so the collision is *resolved* rather than
    /// silently shared. Returns [`OnionError::VipSpaceExhausted`] only if
    /// the entire `127.128.0.0/16` space (65,534 slots) is occupied.
    fn allocate_vip(&self, id: &ServiceId) -> Result<VirtualIP, OnionError> {
        let base = id.qualified();
        let vip = VirtualIP::from_qualified(&base);
        if !self.allocated_vips.contains(&vip) {
            return Ok(vip);
        }
        // Deterministic linear probe over salted successors. Capped at the
        // usable address count so a genuinely full space terminates.
        for attempt in 1..VIP_SPACE {
            let candidate = VirtualIP::from_qualified(&format!("{base}#{attempt}"));
            if !self.allocated_vips.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Err(OnionError::VipSpaceExhausted { name: base })
    }

    /// Add a backend instance to a registered service.
    pub fn add_backend(
        &mut self,
        id: &ServiceId,
        backend: BackendInstance,
    ) -> Result<(), OnionError> {
        let key = id.qualified();
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or(OnionError::ServiceNotFound { name: key })?;

        if entry.backends.len() >= MAX_BACKENDS {
            return Err(OnionError::TooManyBackends {
                app_name: id.qualified(),
            });
        }

        // Replace if instance_id already exists (restart scenario)
        if let Some(existing) = entry
            .backends
            .iter_mut()
            .find(|b| b.instance_id == backend.instance_id)
        {
            *existing = backend;
        } else {
            entry.backends.push(backend);
        }

        Ok(())
    }

    /// Remove a backend instance from a service.
    pub fn remove_backend(&mut self, id: &ServiceId, instance_id: &str) -> Result<(), OnionError> {
        let key = id.qualified();
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or(OnionError::ServiceNotFound { name: key })?;

        let before = entry.backends.len();
        entry.backends.retain(|b| b.instance_id != instance_id);

        if entry.backends.len() == before {
            return Err(OnionError::BackendNotFound {
                app_name: id.qualified(),
                instance_id: instance_id.to_string(),
            });
        }

        Ok(())
    }

    /// Update the health status of a backend.
    pub fn set_backend_health(
        &mut self,
        id: &ServiceId,
        instance_id: &str,
        healthy: bool,
    ) -> Result<(), OnionError> {
        let key = id.qualified();
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or(OnionError::ServiceNotFound { name: key })?;

        let backend = entry
            .backends
            .iter_mut()
            .find(|b| b.instance_id == instance_id)
            .ok_or_else(|| OnionError::BackendNotFound {
                app_name: id.qualified(),
                instance_id: instance_id.to_string(),
            })?;

        backend.healthy = healthy;
        Ok(())
    }

    /// Remove a service from the map entirely, releasing its VIP so the
    /// address returns to the free pool (previously VIPs lingered forever).
    pub fn unregister(&mut self, id: &ServiceId) -> Result<ServiceEntry, OnionError> {
        let key = id.qualified();
        let entry = self
            .entries
            .remove(&key)
            .ok_or(OnionError::ServiceNotFound { name: key })?;
        self.allocated_vips.remove(&entry.vip);
        Ok(entry)
    }

    /// Look up a service by its namespace-qualified identity.
    pub fn resolve(&self, id: &ServiceId) -> Option<&ServiceEntry> {
        self.entries.get(&id.qualified())
    }

    /// Look up a service by bare name, returning the first match in any
    /// namespace. Kept for the CLI `relish resolve <name>` and Smoker
    /// fault paths, which target a service by name only. Prefer
    /// [`ServiceMap::resolve`] everywhere a namespace is known.
    pub fn resolve_by_name(&self, app_name: &str) -> Option<&ServiceEntry> {
        self.entries.values().find(|e| e.app_name == app_name)
    }

    /// List all registered services.
    pub fn resolve_all(&self) -> Vec<&ServiceEntry> {
        self.entries.values().collect()
    }

    /// Produce a resolution view that overlays a cluster-wide endpoint
    /// catalogue onto the local map, so DNS and ingress can resolve
    /// services whose backends live on other nodes (12b.4).
    ///
    /// This does *not* mutate the local map: the local map stays the source
    /// of truth for what this node runs and what it syncs to the eBPF
    /// backend map. The returned map is a read-only merge for the DNS /
    /// routing watch snapshot:
    ///
    /// - a service only in the catalogue (all its backends elsewhere) is
    ///   added wholesale, using the catalogue's cluster-agreed VIP;
    /// - a service present both locally and in the catalogue keeps the
    ///   local entry (and its VIP) and gains any catalogue backends that
    ///   aren't already local, deduplicated by `instance_id`.
    ///
    /// The catalogue is the leader's aggregate, which already includes this
    /// node's own backends, so the dedupe keeps a local instance from
    /// appearing twice.
    pub fn with_cluster_catalog(&self, catalog: &super::catalog::EndpointCatalog) -> ServiceMap {
        let mut merged = self.clone();
        for (qualified, service) in &catalog.services {
            match merged.entries.get_mut(qualified) {
                Some(local) => {
                    for backend in &service.backends {
                        let instance_id = catalog_instance_id(backend);
                        if local.backends.iter().any(|b| b.instance_id == instance_id) {
                            continue;
                        }
                        local.backends.push(BackendInstance {
                            instance_id,
                            node_ip: backend.node_ip,
                            host_port: backend.host_port,
                            healthy: backend.healthy,
                        });
                    }
                }
                None => {
                    let Some(id) = ServiceId::parse(qualified) else {
                        continue;
                    };
                    let entry = ServiceEntry {
                        app_name: id.name.clone(),
                        namespace: id.namespace.clone(),
                        namespace_id: name_to_id(&id.namespace),
                        app_id: name_to_id(&id.name),
                        vip: service.vip,
                        port: service.port,
                        backends: service
                            .backends
                            .iter()
                            .map(|b| BackendInstance {
                                instance_id: catalog_instance_id(b),
                                node_ip: b.node_ip,
                                host_port: b.host_port,
                                healthy: b.healthy,
                            })
                            .collect(),
                        firewall_allow_from: None,
                    };
                    merged.entries.insert(qualified.clone(), entry);
                    merged.allocated_vips.insert(service.vip);
                }
            }
        }
        merged
    }

    /// Number of registered services.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A stable synthetic instance id for a catalogue backend.
///
/// The catalogue doesn't carry the original per-instance id, but the
/// merge needs a key to deduplicate against local backends. `{node}:{ip}:
/// {port}` is unique per backend and stable across ticks, so a remote
/// backend never duplicates and a local one it overlaps is matched by the
/// local map's own `add_backend` dedupe path (both key on this string when
/// the local id was built the same way — see the agent's merge).
fn catalog_instance_id(backend: &super::catalog::CatalogBackend) -> String {
    format!(
        "{}:{}:{}",
        backend.node_id, backend.node_ip, backend.host_port
    )
}

/// Usable VIP slots in `127.128.0.0/16` (65,534: excludes .0.0 and .255.255).
const VIP_SPACE: u32 = 65_534;

impl Default for ServiceMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sid(namespace: &str, name: &str) -> ServiceId {
        ServiceId::new(namespace, name)
    }

    fn test_backend(id: &str, ip: [u8; 4], port: u16) -> BackendInstance {
        BackendInstance {
            instance_id: id.to_string(),
            node_ip: Ipv4Addr::from(ip),
            host_port: port,
            healthy: true,
        }
    }

    #[test]
    fn register_and_resolve() {
        let mut map = ServiceMap::new();
        let vip = map.register(&sid("default", "redis"), 6379, None).unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.app_name, "redis");
        assert_eq!(entry.namespace, "default");
        assert_eq!(entry.port, 6379);
        assert_eq!(entry.vip, vip);
        assert!(entry.backends.is_empty());
    }

    #[test]
    fn register_generates_deterministic_vip() {
        let mut map = ServiceMap::new();
        let vip = map.register(&sid("default", "redis"), 6379, None).unwrap();
        assert_eq!(vip, VirtualIP::from_service_id(&sid("default", "redis")));
    }

    #[test]
    fn same_name_two_namespaces_get_distinct_vips_and_resolve_independently() {
        // The D3/codex-M1 regression, at the map level.
        let mut map = ServiceMap::new();
        let a = map.register(&sid("default", "api"), 3000, None).unwrap();
        let b = map.register(&sid("payments", "api"), 3000, None).unwrap();
        assert_ne!(a, b, "same-named apps in two namespaces shared a VIP");

        // Both entries coexist and resolve to their own VIP.
        assert_eq!(map.resolve(&sid("default", "api")).unwrap().vip, a);
        assert_eq!(map.resolve(&sid("payments", "api")).unwrap().vip, b);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn register_duplicate_errors() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        let err = map
            .register(&sid("default", "redis"), 6379, None)
            .unwrap_err();
        assert!(matches!(err, OnionError::AlreadyRegistered { .. }));
    }

    #[test]
    fn unregister_releases_the_vip_for_reuse() {
        let mut map = ServiceMap::new();
        let vip = map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.unregister(&sid("default", "redis")).unwrap();

        // A different service that happens to hash to the freed VIP can
        // now take it: the address is back in the pool.
        assert!(
            !map.allocated_vips.contains(&vip),
            "VIP was not released on unregister"
        );
    }

    #[test]
    fn colliding_vip_is_resolved_not_shared() {
        // Force a collision by pre-occupying the VIP a fresh service would
        // hash to, then registering that service. It must get a *different*
        // VIP, never the occupied one.
        let mut map = ServiceMap::new();
        let victim = sid("default", "redis");
        let natural = VirtualIP::from_service_id(&victim);
        map.allocated_vips.insert(natural);

        let assigned = map.allocate_vip(&victim).unwrap();
        assert_ne!(assigned, natural, "collision was silently shared");
    }

    #[test]
    fn add_backend_appears_in_entry() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 30891),
        )
        .unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].instance_id, "redis-0");
        assert_eq!(entry.backends[0].host_port, 30891);
    }

    #[test]
    fn add_backend_replaces_on_same_instance_id() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 30891),
        )
        .unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 31000),
        )
        .unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].host_port, 31000);
    }

    #[test]
    fn add_backend_to_nonexistent_service_errors() {
        let mut map = ServiceMap::new();
        let err = map
            .add_backend(
                &sid("default", "nope"),
                test_backend("nope-0", [10, 0, 2, 2], 30891),
            )
            .unwrap_err();
        assert!(matches!(err, OnionError::ServiceNotFound { .. }));
    }

    #[test]
    fn remove_backend_disappears() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 30891),
        )
        .unwrap();
        map.remove_backend(&sid("default", "redis"), "redis-0")
            .unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert!(entry.backends.is_empty());
    }

    #[test]
    fn remove_nonexistent_backend_errors() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        let err = map
            .remove_backend(&sid("default", "redis"), "redis-99")
            .unwrap_err();
        assert!(matches!(err, OnionError::BackendNotFound { .. }));
    }

    #[test]
    fn set_backend_health_updates_flag() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 30891),
        )
        .unwrap();

        map.set_backend_health(&sid("default", "redis"), "redis-0", false)
            .unwrap();
        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert!(!entry.backends[0].healthy);

        map.set_backend_health(&sid("default", "redis"), "redis-0", true)
            .unwrap();
        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert!(entry.backends[0].healthy);
    }

    #[test]
    fn unregister_removes_everything() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.add_backend(
            &sid("default", "redis"),
            test_backend("redis-0", [10, 0, 2, 2], 30891),
        )
        .unwrap();

        let removed = map.unregister(&sid("default", "redis")).unwrap();
        assert_eq!(removed.app_name, "redis");
        assert!(map.resolve(&sid("default", "redis")).is_none());
    }

    #[test]
    fn unregister_nonexistent_errors() {
        let mut map = ServiceMap::new();
        let err = map.unregister(&sid("default", "nope")).unwrap_err();
        assert!(matches!(err, OnionError::ServiceNotFound { .. }));
    }

    #[test]
    fn resolve_nonexistent_returns_none() {
        let map = ServiceMap::new();
        assert!(map.resolve(&sid("default", "nope")).is_none());
    }

    #[test]
    fn resolve_by_name_finds_across_namespace() {
        let mut map = ServiceMap::new();
        map.register(&sid("payments", "api"), 3000, None).unwrap();
        assert!(map.resolve_by_name("api").is_some());
        assert!(map.resolve_by_name("ghost").is_none());
    }

    #[test]
    fn resolve_all_returns_all_registered() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.register(&sid("default", "web"), 8080, None).unwrap();
        map.register(&sid("prod", "api"), 3000, None).unwrap();

        let all = map.resolve_all();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn multiple_backends_round_trip() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();

        for i in 0..5 {
            map.add_backend(
                &sid("default", "redis"),
                test_backend(&format!("redis-{i}"), [10, 0, 2, i as u8 + 2], 30000 + i),
            )
            .unwrap();
        }

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.backends.len(), 5);
    }

    #[test]
    fn too_many_backends_errors() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();

        for i in 0..MAX_BACKENDS {
            map.add_backend(
                &sid("default", "redis"),
                test_backend(
                    &format!("redis-{i}"),
                    [10, 0, 2, i as u8 + 2],
                    30000 + i as u16,
                ),
            )
            .unwrap();
        }

        let err = map
            .add_backend(
                &sid("default", "redis"),
                test_backend("redis-extra", [10, 0, 2, 100], 31000),
            )
            .unwrap_err();
        assert!(matches!(err, OnionError::TooManyBackends { .. }));
    }

    #[test]
    fn namespace_id_deterministic() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();
        map.register(&sid("default", "web"), 8080, None).unwrap();

        let redis = map.resolve(&sid("default", "redis")).unwrap();
        let web = map.resolve(&sid("default", "web")).unwrap();
        assert_eq!(redis.namespace_id, web.namespace_id);
    }

    #[test]
    fn app_id_deterministic() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.app_id, name_to_id("redis"));
    }

    #[test]
    fn len_and_is_empty() {
        let mut map = ServiceMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        map.register(&sid("default", "redis"), 6379, None).unwrap();
        assert!(!map.is_empty());
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn with_cluster_catalog_adds_remote_only_service() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};

        // Local map has nothing; the catalogue has a service whose backends
        // all live on another node. The merged view must resolve it.
        let local = ServiceMap::new();
        let catalog = EndpointCatalog::rebuild([(
            sid("payments", "api"),
            3000,
            vec![CatalogBackend {
                node_id: "node-b".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 0, 2),
                host_port: 30002,
                healthy: true,
            }],
        )]);
        let merged = local.with_cluster_catalog(&catalog);

        let entry = merged.resolve(&sid("payments", "api")).unwrap();
        assert_eq!(entry.namespace, "payments");
        assert_eq!(entry.port, 3000);
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].node_ip, Ipv4Addr::new(10, 0, 0, 2));
        // The VIP is the cluster-agreed one from the catalogue.
        assert_eq!(
            entry.vip,
            catalog.resolve(&sid("payments", "api")).unwrap().vip
        );
    }

    #[test]
    fn with_cluster_catalog_merges_remote_backend_into_local_service() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};

        // The service runs locally with one backend; the catalogue also lists
        // a backend on another node. The merged view must have both.
        let mut local = ServiceMap::new();
        local.register(&sid("default", "web"), 8080, None).unwrap();
        local
            .add_backend(
                &sid("default", "web"),
                test_backend("web-0", [10, 0, 0, 1], 30001),
            )
            .unwrap();
        let local_vip = local.resolve(&sid("default", "web")).unwrap().vip;

        let catalog = EndpointCatalog::rebuild([(
            sid("default", "web"),
            8080,
            vec![CatalogBackend {
                node_id: "node-b".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 0, 2),
                host_port: 30002,
                healthy: true,
            }],
        )]);
        let merged = local.with_cluster_catalog(&catalog);

        let entry = merged.resolve(&sid("default", "web")).unwrap();
        // Local VIP is kept; the remote backend is added.
        assert_eq!(entry.vip, local_vip);
        assert_eq!(entry.backends.len(), 2);
        assert!(
            entry
                .backends
                .iter()
                .any(|b| b.node_ip == Ipv4Addr::new(10, 0, 0, 2))
        );
    }

    #[test]
    fn with_cluster_catalog_does_not_duplicate_a_shared_backend() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};

        // The catalogue includes this node's own backend (the leader's
        // aggregate always does). Merging it must not duplicate the entry:
        // the local backend was registered with the same synthetic id.
        let mut local = ServiceMap::new();
        local.register(&sid("default", "web"), 8080, None).unwrap();
        local
            .add_backend(
                &sid("default", "web"),
                BackendInstance {
                    instance_id: "node-a:10.0.0.1:30001".to_string(),
                    node_ip: Ipv4Addr::new(10, 0, 0, 1),
                    host_port: 30001,
                    healthy: true,
                },
            )
            .unwrap();

        let catalog = EndpointCatalog::rebuild([(
            sid("default", "web"),
            8080,
            vec![CatalogBackend {
                node_id: "node-a".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 0, 1),
                host_port: 30001,
                healthy: true,
            }],
        )]);
        let merged = local.with_cluster_catalog(&catalog);

        let entry = merged.resolve(&sid("default", "web")).unwrap();
        assert_eq!(entry.backends.len(), 1, "shared backend was duplicated");
    }
}

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

    /// Restore exact saved allocations without rehashing or publishing routes.
    ///
    /// Reject the entire inventory on conflicting identities, addresses or invalid
    /// backends. Recorded health is historical: callers must reconcile runtime and
    /// kernel ownership before exposing this map to DNS, ingress or kernel hooks.
    pub fn from_snapshot(entries: &[ServiceEntry]) -> Result<Self, OnionError> {
        let mut map = Self::new();
        for entry in entries {
            let id = ServiceId::new(&entry.namespace, &entry.app_name);
            let key = id.qualified();
            let invalid = |reason| OnionError::InvalidSnapshot {
                service: key.clone(),
                reason,
            };
            if !crate::config::valid_workload_label(&entry.namespace)
                || !crate::config::valid_workload_label(&entry.app_name)
                || entry.namespace_id != name_to_id(&entry.namespace)
                || entry.app_id != u32::from(entry.vip.0)
                || !(0x7f80_0001..=0x7f80_fffe).contains(&u32::from(entry.vip.0))
                || entry.port == 0
            {
                return Err(invalid("invalid service identity or allocation"));
            }
            if map.entries.contains_key(&key) || map.allocated_vips.contains(&entry.vip) {
                return Err(invalid("duplicate service or virtual IP owner"));
            }
            if entry.backends.len() > MAX_BACKENDS {
                return Err(invalid("backend capacity exceeded"));
            }
            let mut backend_ids = HashSet::new();
            for backend in &entry.backends {
                if backend.instance_id.is_empty()
                    || backend.host_port == 0
                    || backend.node_ip.is_unspecified()
                    || !backend_ids.insert(&backend.instance_id)
                {
                    return Err(invalid("invalid or duplicate backend"));
                }
            }
            map.allocated_vips.insert(entry.vip);
            map.entries.insert(key, entry.clone());
        }
        Ok(map)
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
            app_id: u32::from(vip.0),
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

        // Updating an existing endpoint does not consume another backend slot.
        if let Some(existing) = entry
            .backends
            .iter_mut()
            .find(|b| b.instance_id == backend.instance_id)
        {
            *existing = backend;
            return Ok(());
        }
        if entry.backends.len() >= MAX_BACKENDS {
            return Err(OnionError::TooManyBackends {
                app_name: id.qualified(),
            });
        }
        entry.backends.push(backend);
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
        self.with_cluster_catalog_excluding_node(catalog, None)
    }

    /// Merge remote published endpoints, keeping local workloads on their
    /// directly reachable container addresses instead of hairpinning host NAT.
    pub fn with_cluster_catalog_excluding_node(
        &self,
        catalog: &super::catalog::EndpointCatalog,
        local_node: Option<&str>,
    ) -> ServiceMap {
        let mut merged = self.clone();
        for (qualified, service) in &catalog.services {
            match merged.entries.get_mut(qualified) {
                Some(local) => {
                    for backend in service
                        .backends
                        .iter()
                        .filter(|backend| Some(backend.node_id.as_str()) != local_node)
                    {
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
                        app_id: u32::from(service.vip.0),
                        vip: service.vip,
                        port: service.port,
                        backends: service
                            .backends
                            .iter()
                            .filter(|backend| Some(backend.node_id.as_str()) != local_node)
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
    fn snapshot_restores_collision_resolved_addresses_in_any_order() {
        let mut seen = HashMap::new();
        let (first, second) = (0..65_535)
            .find_map(|index| {
                let id = sid("default", &format!("service-{index}"));
                seen.insert(VirtualIP::from_service_id(&id), id.clone())
                    .map(|first| (first, id))
            })
            .unwrap();
        let mut map = ServiceMap::new();
        let first_vip = map.register(&first, 8080, None).unwrap();
        let second_vip = map
            .register(&second, 9000, Some(vec!["default/client".into()]))
            .unwrap();
        assert_ne!(first_vip, second_vip);
        let mut backend = test_backend("original-generation", [10, 0, 2, 2], 9000);
        backend.healthy = false;
        map.add_backend(&second, backend).unwrap();
        let snapshot = vec![
            map.resolve(&second).unwrap().clone(),
            map.resolve(&first).unwrap().clone(),
        ];
        let mut recovered = ServiceMap::from_snapshot(&snapshot).unwrap();
        for entry in &snapshot {
            let id = sid(&entry.namespace, &entry.app_name);
            assert_eq!(
                serde_json::to_value(recovered.resolve(&id).unwrap()).unwrap(),
                serde_json::to_value(entry).unwrap()
            );
        }
        recovered.unregister(&second).unwrap();
        assert_eq!(
            recovered.register(&second, 9000, None).unwrap(),
            second_vip,
            "restoration forgot the first service's address reservation"
        );
    }

    #[test]
    fn snapshot_refuses_duplicate_service_or_address_ownership() {
        let mut map = ServiceMap::new();
        let first = sid("default", "first");
        let second = sid("default", "second");
        map.register(&first, 8080, None).unwrap();
        map.register(&second, 9000, None).unwrap();
        let first = map.resolve(&first).unwrap().clone();
        let mut second = map.resolve(&second).unwrap().clone();
        assert!(ServiceMap::from_snapshot(&[first.clone(), first.clone()]).is_err());
        second.vip = first.vip;
        second.app_id = first.app_id;
        assert!(ServiceMap::from_snapshot(&[first, second]).is_err());
    }

    #[test]
    fn snapshot_refuses_invalid_identity_or_backend_evidence() {
        let mut map = ServiceMap::new();
        let id = sid("default", "service");
        map.register(&id, 8080, None).unwrap();
        map.add_backend(&id, test_backend("original", [10, 0, 2, 2], 8080))
            .unwrap();
        let original = map.resolve(&id).unwrap().clone();
        let invalid: &[fn(&mut ServiceEntry)] = &[
            |entry| entry.namespace = "invalid__namespace".into(),
            |entry| entry.app_name = String::new(),
            |entry| entry.namespace_id ^= 1,
            |entry| entry.app_id ^= 1,
            |entry| {
                entry.vip = VirtualIP(Ipv4Addr::new(192, 0, 2, 1));
                entry.app_id = u32::from(entry.vip.0);
            },
            |entry| {
                entry.vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 0));
                entry.app_id = u32::from(entry.vip.0);
            },
            |entry| {
                entry.vip = VirtualIP(Ipv4Addr::new(127, 128, 255, 255));
                entry.app_id = u32::from(entry.vip.0);
            },
            |entry| entry.port = 0,
            |entry| entry.backends.push(entry.backends[0].clone()),
            |entry| entry.backends = vec![entry.backends[0].clone(); MAX_BACKENDS + 1],
            |entry| entry.backends[0].instance_id.clear(),
            |entry| entry.backends[0].host_port = 0,
            |entry| entry.backends[0].node_ip = Ipv4Addr::UNSPECIFIED,
        ];
        for (index, invalidate) in invalid.iter().enumerate() {
            let mut damaged = original.clone();
            invalidate(&mut damaged);
            assert!(
                ServiceMap::from_snapshot(&[damaged]).is_err(),
                "accepted invalid snapshot case {index}"
            );
        }
        assert!(ServiceMap::from_snapshot(&[]).unwrap().is_empty());
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
    fn replacing_a_backend_at_capacity_does_not_consume_another_slot() {
        let mut map = ServiceMap::new();
        let service = sid("default", "api");
        map.register(&service, 8080, None).unwrap();
        for index in 0..MAX_BACKENDS {
            map.add_backend(
                &service,
                test_backend(&format!("api-{index}"), [10, 0, 2, 2], 30000 + index as u16),
            )
            .unwrap();
        }
        let mut replacement = test_backend("api-0", [10, 0, 3, 3], 31000);
        replacement.healthy = false;
        map.add_backend(&service, replacement).unwrap();
        let entry = map.resolve(&service).unwrap();
        assert_eq!(entry.backends.len(), MAX_BACKENDS);
        assert_eq!(entry.backends[0].node_ip, Ipv4Addr::new(10, 0, 3, 3));
        assert_eq!(entry.backends[0].host_port, 31000);
        assert!(!entry.backends[0].healthy);
        let original = serde_json::to_value(entry).unwrap();
        assert!(matches!(
            map.add_backend(&service, test_backend("overflow", [10, 0, 4, 4], 32000)),
            Err(OnionError::TooManyBackends { .. })
        ));
        assert_eq!(
            serde_json::to_value(map.resolve(&service).unwrap()).unwrap(),
            original
        );
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
    fn same_named_destinations_have_distinct_firewall_identities() {
        let mut map = ServiceMap::new();
        map.register(&sid("permitted", "database"), 5432, None)
            .unwrap();
        map.register(&sid("private", "database"), 5432, None)
            .unwrap();
        assert_ne!(
            map.resolve(&sid("permitted", "database")).unwrap().app_id,
            map.resolve(&sid("private", "database")).unwrap().app_id,
        );
    }

    #[test]
    fn destination_identity_uses_the_collision_resolved_vip() {
        let mut map = ServiceMap::new();
        let id = sid("default", "redis");
        let natural = VirtualIP::from_service_id(&id);
        map.allocated_vips.insert(natural);
        let assigned = map.register(&id, 6379, None).unwrap();
        assert_ne!(assigned, natural);
        assert_eq!(map.resolve(&id).unwrap().app_id, u32::from(assigned.0));
    }

    #[test]
    fn remote_destination_identity_uses_the_catalogue_allocation() {
        use crate::onion::catalog::{CatalogService, EndpointCatalog};
        let id = sid("remote", "redis");
        let assigned = VirtualIP(Ipv4Addr::new(127, 128, 7, 19));
        assert_ne!(assigned, VirtualIP::from_service_id(&id));
        let mut catalog = EndpointCatalog::new();
        catalog.services.insert(
            id.qualified(),
            CatalogService {
                vip: assigned,
                port: 6379,
                backends: Vec::new(),
            },
        );
        let merged = ServiceMap::new().with_cluster_catalog(&catalog);
        assert_eq!(merged.resolve(&id).unwrap().app_id, u32::from(assigned.0));
    }

    #[test]
    fn app_id_deterministic() {
        let mut map = ServiceMap::new();
        map.register(&sid("default", "redis"), 6379, None).unwrap();

        let entry = map.resolve(&sid("default", "redis")).unwrap();
        assert_eq!(entry.app_id, u32::from(entry.vip.0));
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
    fn local_node_uses_direct_backends_instead_of_its_own_published_ports() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};
        let local = ServiceMap::new();
        let id = ServiceId::new("default", "web");
        let catalog = EndpointCatalog::rebuild([(
            id.clone(),
            8080,
            vec![
                CatalogBackend {
                    node_id: "here".into(),
                    node_ip: "192.168.1.1".parse().unwrap(),
                    host_port: 30001,
                    healthy: true,
                },
                CatalogBackend {
                    node_id: "there".into(),
                    node_ip: "192.168.1.2".parse().unwrap(),
                    host_port: 30002,
                    healthy: true,
                },
            ],
        )]);
        let merged = local.with_cluster_catalog_excluding_node(&catalog, Some("here"));
        let entry = merged.resolve(&id).unwrap();
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].host_port, 30002);
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

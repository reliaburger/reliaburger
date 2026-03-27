/// Userspace service map for Onion.
///
/// Maintains the mapping from app names to virtual IPs and backend
/// lists. This is the source of truth that gets synced to BPF maps
/// on Linux. On all platforms, it powers `relish resolve`.
use std::collections::HashMap;

use super::types::{BackendInstance, MAX_BACKENDS, OnionError, ServiceEntry};
use super::vip::{VirtualIP, name_to_id};

/// The service map: app names to their service entries.
///
/// All mutations go through this struct's methods, which enforce
/// invariants (max backends, unique instance IDs, etc.).
pub struct ServiceMap {
    entries: HashMap<String, ServiceEntry>,
}

impl ServiceMap {
    /// Create an empty service map.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a new app in the service map.
    ///
    /// Computes the VIP deterministically from the app name. Starts
    /// with an empty backend list — backends are added as instances
    /// reach the Running state.
    pub fn register_app(
        &mut self,
        app_name: &str,
        namespace: &str,
        port: u16,
        firewall_allow_from: Option<Vec<String>>,
    ) -> Result<VirtualIP, OnionError> {
        if self.entries.contains_key(app_name) {
            return Err(OnionError::AlreadyRegistered {
                name: app_name.to_string(),
            });
        }

        let vip = VirtualIP::from_app_name(app_name);

        let entry = ServiceEntry {
            app_name: app_name.to_string(),
            namespace: namespace.to_string(),
            namespace_id: name_to_id(namespace),
            app_id: name_to_id(app_name),
            vip,
            port,
            backends: Vec::new(),
            firewall_allow_from,
        };

        self.entries.insert(app_name.to_string(), entry);
        Ok(vip)
    }

    /// Add a backend instance to a registered service.
    pub fn add_backend(
        &mut self,
        app_name: &str,
        backend: BackendInstance,
    ) -> Result<(), OnionError> {
        let entry = self
            .entries
            .get_mut(app_name)
            .ok_or_else(|| OnionError::ServiceNotFound {
                name: app_name.to_string(),
            })?;

        if entry.backends.len() >= MAX_BACKENDS {
            return Err(OnionError::TooManyBackends {
                app_name: app_name.to_string(),
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
    pub fn remove_backend(&mut self, app_name: &str, instance_id: &str) -> Result<(), OnionError> {
        let entry = self
            .entries
            .get_mut(app_name)
            .ok_or_else(|| OnionError::ServiceNotFound {
                name: app_name.to_string(),
            })?;

        let before = entry.backends.len();
        entry.backends.retain(|b| b.instance_id != instance_id);

        if entry.backends.len() == before {
            return Err(OnionError::BackendNotFound {
                app_name: app_name.to_string(),
                instance_id: instance_id.to_string(),
            });
        }

        Ok(())
    }

    /// Update the health status of a backend.
    pub fn set_backend_health(
        &mut self,
        app_name: &str,
        instance_id: &str,
        healthy: bool,
    ) -> Result<(), OnionError> {
        let entry = self
            .entries
            .get_mut(app_name)
            .ok_or_else(|| OnionError::ServiceNotFound {
                name: app_name.to_string(),
            })?;

        let backend = entry
            .backends
            .iter_mut()
            .find(|b| b.instance_id == instance_id)
            .ok_or_else(|| OnionError::BackendNotFound {
                app_name: app_name.to_string(),
                instance_id: instance_id.to_string(),
            })?;

        backend.healthy = healthy;
        Ok(())
    }

    /// Remove an app from the service map entirely.
    pub fn unregister_app(&mut self, app_name: &str) -> Result<ServiceEntry, OnionError> {
        self.entries
            .remove(app_name)
            .ok_or_else(|| OnionError::ServiceNotFound {
                name: app_name.to_string(),
            })
    }

    /// Look up a service by app name.
    pub fn resolve(&self, app_name: &str) -> Option<&ServiceEntry> {
        self.entries.get(app_name)
    }

    /// List all registered services.
    pub fn resolve_all(&self) -> Vec<&ServiceEntry> {
        self.entries.values().collect()
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

impl Default for ServiceMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
        let vip = map.register_app("redis", "default", 6379, None).unwrap();

        let entry = map.resolve("redis").unwrap();
        assert_eq!(entry.app_name, "redis");
        assert_eq!(entry.namespace, "default");
        assert_eq!(entry.port, 6379);
        assert_eq!(entry.vip, vip);
        assert!(entry.backends.is_empty());
    }

    #[test]
    fn register_generates_deterministic_vip() {
        let mut map = ServiceMap::new();
        let vip = map.register_app("redis", "default", 6379, None).unwrap();
        assert_eq!(vip, VirtualIP::from_app_name("redis"));
    }

    #[test]
    fn register_duplicate_errors() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let err = map
            .register_app("redis", "default", 6379, None)
            .unwrap_err();
        assert!(matches!(err, OnionError::AlreadyRegistered { .. }));
    }

    #[test]
    fn add_backend_appears_in_entry() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 30891))
            .unwrap();

        let entry = map.resolve("redis").unwrap();
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].instance_id, "redis-0");
        assert_eq!(entry.backends[0].host_port, 30891);
    }

    #[test]
    fn add_backend_replaces_on_same_instance_id() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 30891))
            .unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 31000))
            .unwrap();

        let entry = map.resolve("redis").unwrap();
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].host_port, 31000);
    }

    #[test]
    fn add_backend_to_nonexistent_service_errors() {
        let mut map = ServiceMap::new();
        let err = map
            .add_backend("nope", test_backend("nope-0", [10, 0, 2, 2], 30891))
            .unwrap_err();
        assert!(matches!(err, OnionError::ServiceNotFound { .. }));
    }

    #[test]
    fn remove_backend_disappears() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 30891))
            .unwrap();
        map.remove_backend("redis", "redis-0").unwrap();

        let entry = map.resolve("redis").unwrap();
        assert!(entry.backends.is_empty());
    }

    #[test]
    fn remove_nonexistent_backend_errors() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let err = map.remove_backend("redis", "redis-99").unwrap_err();
        assert!(matches!(err, OnionError::BackendNotFound { .. }));
    }

    #[test]
    fn set_backend_health_updates_flag() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 30891))
            .unwrap();

        map.set_backend_health("redis", "redis-0", false).unwrap();
        let entry = map.resolve("redis").unwrap();
        assert!(!entry.backends[0].healthy);

        map.set_backend_health("redis", "redis-0", true).unwrap();
        let entry = map.resolve("redis").unwrap();
        assert!(entry.backends[0].healthy);
    }

    #[test]
    fn unregister_removes_everything() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.add_backend("redis", test_backend("redis-0", [10, 0, 2, 2], 30891))
            .unwrap();

        let removed = map.unregister_app("redis").unwrap();
        assert_eq!(removed.app_name, "redis");
        assert!(map.resolve("redis").is_none());
    }

    #[test]
    fn unregister_nonexistent_errors() {
        let mut map = ServiceMap::new();
        let err = map.unregister_app("nope").unwrap_err();
        assert!(matches!(err, OnionError::ServiceNotFound { .. }));
    }

    #[test]
    fn resolve_nonexistent_returns_none() {
        let map = ServiceMap::new();
        assert!(map.resolve("nope").is_none());
    }

    #[test]
    fn resolve_all_returns_all_registered() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.register_app("web", "default", 8080, None).unwrap();
        map.register_app("api", "prod", 3000, None).unwrap();

        let all = map.resolve_all();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn multiple_backends_round_trip() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();

        for i in 0..5 {
            map.add_backend(
                "redis",
                test_backend(&format!("redis-{i}"), [10, 0, 2, i as u8 + 2], 30000 + i),
            )
            .unwrap();
        }

        let entry = map.resolve("redis").unwrap();
        assert_eq!(entry.backends.len(), 5);
    }

    #[test]
    fn too_many_backends_errors() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();

        for i in 0..MAX_BACKENDS {
            map.add_backend(
                "redis",
                test_backend(
                    &format!("redis-{i}"),
                    [10, 0, 2, i as u8 + 2],
                    30000 + i as u16,
                ),
            )
            .unwrap();
        }

        let err = map
            .add_backend("redis", test_backend("redis-extra", [10, 0, 2, 100], 31000))
            .unwrap_err();
        assert!(matches!(err, OnionError::TooManyBackends { .. }));
    }

    #[test]
    fn namespace_id_deterministic() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.register_app("web", "default", 8080, None).unwrap();

        let redis = map.resolve("redis").unwrap();
        let web = map.resolve("web").unwrap();
        assert_eq!(redis.namespace_id, web.namespace_id);
    }

    #[test]
    fn app_id_deterministic() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();

        let entry = map.resolve("redis").unwrap();
        assert_eq!(entry.app_id, name_to_id("redis"));
    }

    #[test]
    fn len_and_is_empty() {
        let mut map = ServiceMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        map.register_app("redis", "default", 6379, None).unwrap();
        assert!(!map.is_empty());
        assert_eq!(map.len(), 1);
    }
}

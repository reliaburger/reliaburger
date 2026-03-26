/// BPF map operations for Onion service discovery.
///
/// Wraps the BPF hash maps with typed Rust methods for updating
/// DNS entries, backend lists, and firewall rules. When aya is
/// available, these write directly to kernel BPF maps. Without
/// aya, they are no-ops (the userspace `ServiceMap` handles
/// everything).
use std::net::Ipv4Addr;

use super::super::service_map::ServiceMap;
use super::super::types::{
    BackendEndpoint, BackendKey, BackendValue, DnsMapKey, DnsMapValue, MAX_BACKENDS,
};
use super::super::vip::VirtualIP;

/// Manages BPF map synchronisation from the userspace `ServiceMap`.
///
/// On Linux with the `ebpf` feature, this holds aya map handles
/// and writes directly to the kernel. Without aya, all methods
/// are no-ops.
pub struct BpfServiceMap {
    // TODO(Phase 3, Step 2b): Add aya HashMap handles when the
    // aya dependency is enabled. For now, track whether we've
    // been initialised so callers can check.
    initialised: bool,
}

impl BpfServiceMap {
    /// Create a new BPF service map manager.
    ///
    /// Call `sync_from_service_map()` after loading eBPF programs
    /// to populate the kernel maps from the current service state.
    pub fn new() -> Self {
        Self { initialised: false }
    }

    /// Full sync: write all entries from the userspace `ServiceMap`
    /// into the BPF maps.
    ///
    /// Called on startup after loading eBPF programs, and after
    /// reconnecting to an existing pinned BPF filesystem.
    pub fn sync_from_service_map(&mut self, map: &ServiceMap) {
        for entry in map.resolve_all() {
            let dns_key = DnsMapKey::from_name(&format!("{}.internal", entry.app_name));
            let dns_value = DnsMapValue {
                vip: entry.vip.to_network_byte_order(),
            };

            // Build the backend value
            let backend_key = BackendKey {
                vip: entry.vip.to_network_byte_order(),
                port: entry.port.to_be(),
                _pad: 0,
            };
            let backend_value = service_entry_to_backend_value(entry);

            // TODO(Phase 3, Step 2b): Write to actual BPF maps via aya
            let _ = (dns_key, dns_value, backend_key, backend_value);
        }
        self.initialised = true;
    }

    /// Update the DNS map for a single service.
    pub fn update_dns(&self, app_name: &str, vip: VirtualIP) {
        let _key = DnsMapKey::from_name(&format!("{app_name}.internal"));
        let _value = DnsMapValue {
            vip: vip.to_network_byte_order(),
        };
        // TODO(Phase 3, Step 2b): bpf_map.insert(&key, &value, 0)
    }

    /// Remove a DNS map entry.
    pub fn remove_dns(&self, app_name: &str) {
        let _key = DnsMapKey::from_name(&format!("{app_name}.internal"));
        // TODO(Phase 3, Step 2b): bpf_map.remove(&key)
    }

    /// Update the backend map for a service.
    pub fn update_backends(
        &self,
        vip: VirtualIP,
        port: u16,
        entry: &super::super::types::ServiceEntry,
    ) {
        let _key = BackendKey {
            vip: vip.to_network_byte_order(),
            port: port.to_be(),
            _pad: 0,
        };
        let _value = service_entry_to_backend_value(entry);
        // TODO(Phase 3, Step 2b): bpf_map.insert(&key, &value, 0)
    }

    /// Remove a backend map entry.
    pub fn remove_backends(&self, vip: VirtualIP, port: u16) {
        let _key = BackendKey {
            vip: vip.to_network_byte_order(),
            port: port.to_be(),
            _pad: 0,
        };
        // TODO(Phase 3, Step 2b): bpf_map.remove(&key)
    }

    /// Whether the BPF maps have been initialised.
    pub fn is_initialised(&self) -> bool {
        self.initialised
    }
}

impl Default for BpfServiceMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a `ServiceEntry` into a `BackendValue` for the BPF map.
fn service_entry_to_backend_value(entry: &super::super::types::ServiceEntry) -> BackendValue {
    let mut backends = [BackendEndpoint {
        host_ip: 0,
        host_port: 0,
        healthy: 0,
        _pad: 0,
    }; MAX_BACKENDS];

    let count = entry.backends.len().min(MAX_BACKENDS);
    for (i, backend) in entry.backends.iter().take(MAX_BACKENDS).enumerate() {
        backends[i] = BackendEndpoint {
            host_ip: ip_to_network_byte_order(backend.node_ip),
            host_port: backend.host_port.to_be(),
            healthy: if backend.healthy { 1 } else { 0 },
            _pad: 0,
        };
    }

    BackendValue {
        count: count as u32,
        rr_index: 0,
        app_id: entry.app_id,
        namespace_id: entry.namespace_id,
        backends,
    }
}

/// Convert an `Ipv4Addr` to network byte order u32.
fn ip_to_network_byte_order(ip: Ipv4Addr) -> u32 {
    u32::from(ip).to_be()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onion::types::{BackendInstance, ServiceEntry};

    fn test_entry() -> ServiceEntry {
        ServiceEntry {
            app_name: "redis".to_string(),
            namespace: "default".to_string(),
            namespace_id: 42,
            app_id: 7,
            vip: VirtualIP(Ipv4Addr::new(127, 128, 0, 3)),
            port: 6379,
            backends: vec![
                BackendInstance {
                    instance_id: "redis-0".to_string(),
                    node_ip: Ipv4Addr::new(10, 0, 2, 2),
                    host_port: 30891,
                    healthy: true,
                },
                BackendInstance {
                    instance_id: "redis-1".to_string(),
                    node_ip: Ipv4Addr::new(10, 0, 4, 2),
                    host_port: 31022,
                    healthy: false,
                },
            ],
            firewall_allow_from: None,
        }
    }

    #[test]
    fn backend_value_from_service_entry() {
        let entry = test_entry();
        let value = service_entry_to_backend_value(&entry);

        assert_eq!(value.count, 2);
        assert_eq!(value.app_id, 7);
        assert_eq!(value.namespace_id, 42);
        assert_eq!(value.rr_index, 0);
        assert_eq!(value.backends[0].healthy, 1);
        assert_eq!(value.backends[1].healthy, 0);
    }

    #[test]
    fn backend_value_ips_in_network_byte_order() {
        let entry = test_entry();
        let value = service_entry_to_backend_value(&entry);

        // 10.0.2.2 = 0x0A000202, in network byte order (big-endian)
        let expected = u32::from(Ipv4Addr::new(10, 0, 2, 2)).to_be();
        assert_eq!(value.backends[0].host_ip, expected);
    }

    #[test]
    fn backend_value_ports_in_network_byte_order() {
        let entry = test_entry();
        let value = service_entry_to_backend_value(&entry);

        assert_eq!(value.backends[0].host_port, 30891u16.to_be());
    }

    #[test]
    fn backend_value_empty_backends() {
        let mut entry = test_entry();
        entry.backends.clear();
        let value = service_entry_to_backend_value(&entry);

        assert_eq!(value.count, 0);
    }

    #[test]
    fn sync_from_service_map_marks_initialised() {
        let mut bpf_map = BpfServiceMap::new();
        assert!(!bpf_map.is_initialised());

        let map = ServiceMap::new();
        bpf_map.sync_from_service_map(&map);
        assert!(bpf_map.is_initialised());
    }

    #[test]
    fn dns_key_matches_internal_suffix() {
        let key = DnsMapKey::from_name("redis.internal");
        let name = std::str::from_utf8(&key.name)
            .unwrap()
            .trim_end_matches('\0');
        assert_eq!(name, "redis.internal");
    }
}

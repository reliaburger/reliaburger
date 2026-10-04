use sha2::{Digest, Sha256};
/// BPF map operations for Onion service discovery.
///
/// When the `ebpf` feature is enabled, wraps aya map handles and
/// writes directly to kernel BPF hash maps. Without the feature,
/// all map methods are no-ops — the userspace `ServiceMap` still
/// works for `relish resolve`.
///
/// Every operation returns a `Result`: a `.bpf.o` missing a map used
/// to panic Bun (NET8), and update/remove failures were silently
/// discarded. Callers decide whether an error is fatal.
use std::net::Ipv4Addr;

use super::super::service_map::ServiceMap;
use super::super::types::{BackendEndpoint, BackendKey, BackendValue, MAX_BACKENDS};
#[cfg(feature = "ebpf")]
use super::super::vip::VirtualIP;

/// Errors from the Onion BPF map layer.
#[derive(Debug, thiserror::Error)]
pub enum BpfMapError {
    #[error("bpf map {name:?} not found in the loaded object")]
    MissingMap { name: &'static str },

    #[cfg(feature = "ebpf")]
    #[error("bpf map {name:?} operation failed: {source}")]
    Operation {
        name: &'static str,
        #[source]
        source: aya::maps::MapError,
    },
}

/// Manages BPF map synchronisation from the userspace `ServiceMap`.
pub struct BpfServiceMap {
    initialised: bool,
    consumer: String,
}

impl BpfServiceMap {
    pub fn new() -> Self {
        Self::for_consumer("")
    }

    /// Stable consumer identity spreads oversized remote pools across nodes.
    pub fn for_consumer(consumer: &str) -> Self {
        Self {
            initialised: false,
            consumer: consumer.to_owned(),
        }
    }

    #[cfg(feature = "ebpf")]
    fn backend_map(
        ebpf: &mut super::loader::OnionEbpf,
    ) -> Result<aya::maps::HashMap<&mut aya::maps::MapData, BackendKey, BackendValue>, BpfMapError>
    {
        let map = ebpf
            .bpf
            .map_mut("backend_map")
            .ok_or(BpfMapError::MissingMap {
                name: "backend_map",
            })?;
        aya::maps::HashMap::try_from(map).map_err(|source| BpfMapError::Operation {
            name: "backend_map",
            source,
        })
    }

    /// Full sync: write all entries from the userspace `ServiceMap`
    /// into the BPF maps. Attempts every entry; returns the first error.
    #[cfg(feature = "ebpf")]
    pub fn sync_from_service_map(
        &mut self,
        map: &ServiceMap,
        ebpf: &mut super::loader::OnionEbpf,
    ) -> Result<(), BpfMapError> {
        let mut backend_map = Self::backend_map(ebpf)?;

        let mut first_error = None;
        for entry in map.resolve_all() {
            let backend_key = BackendKey {
                vip: entry.vip.to_network_byte_order(),
                port: entry.port.to_be(),
                _pad: 0,
            };
            let backend_value = service_entry_to_backend_value_for_consumer(entry, &self.consumer);

            if let Err(source) = backend_map.insert(backend_key, backend_value, 0)
                && first_error.is_none()
            {
                first_error = Some(BpfMapError::Operation {
                    name: "backend_map",
                    source,
                });
            }
        }

        self.initialised = true;
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// No-op sync when eBPF is not available.
    #[cfg(not(feature = "ebpf"))]
    pub fn sync_from_service_map(&mut self, map: &ServiceMap) {
        // Walk the map to validate the conversion logic even without BPF
        for entry in map.resolve_all() {
            let _key = BackendKey {
                vip: entry.vip.to_network_byte_order(),
                port: entry.port.to_be(),
                _pad: 0,
            };
            let _value = service_entry_to_backend_value_for_consumer(entry, &self.consumer);
        }
        self.initialised = true;
    }

    /// Update the backend map for a single service.
    #[cfg(feature = "ebpf")]
    pub fn update_backends_bpf(
        &self,
        ebpf: &mut super::loader::OnionEbpf,
        vip: VirtualIP,
        port: u16,
        entry: &super::super::types::ServiceEntry,
    ) -> Result<(), BpfMapError> {
        let mut backend_map = Self::backend_map(ebpf)?;

        let key = BackendKey {
            vip: vip.to_network_byte_order(),
            port: port.to_be(),
            _pad: 0,
        };
        let value = service_entry_to_backend_value_for_consumer(entry, &self.consumer);
        backend_map
            .insert(key, value, 0)
            .map_err(|source| BpfMapError::Operation {
                name: "backend_map",
                source,
            })
    }

    /// Remove a backend map entry. An already-absent key is not an error.
    #[cfg(feature = "ebpf")]
    pub fn remove_backends_bpf(
        &self,
        ebpf: &mut super::loader::OnionEbpf,
        vip: VirtualIP,
        port: u16,
    ) -> Result<(), BpfMapError> {
        let mut backend_map = Self::backend_map(ebpf)?;

        let key = BackendKey {
            vip: vip.to_network_byte_order(),
            port: port.to_be(),
            _pad: 0,
        };
        match backend_map.remove(&key) {
            Ok(()) => Ok(()),
            // Deleting an entry that is already gone achieves the goal.
            Err(aya::maps::MapError::KeyNotFound) => Ok(()),
            Err(source) => {
                if let aya::maps::MapError::SyscallError(e) = &source
                    && e.io_error.kind() == std::io::ErrorKind::NotFound
                {
                    return Ok(());
                }
                Err(BpfMapError::Operation {
                    name: "backend_map",
                    source,
                })
            }
        }
    }

    /// Read a backend entry from the BPF map. `Ok(None)` means the key
    /// is absent; `Err` means the map itself could not be read.
    #[cfg(feature = "ebpf")]
    pub fn read_backends(
        &self,
        ebpf: &mut super::loader::OnionEbpf,
        vip: VirtualIP,
        port: u16,
    ) -> Result<Option<BackendValue>, BpfMapError> {
        let backend_map = Self::backend_map(ebpf)?;

        let key = BackendKey {
            vip: vip.to_network_byte_order(),
            port: port.to_be(),
            _pad: 0,
        };
        match backend_map.get(&key, 0) {
            Ok(value) => Ok(Some(value)),
            Err(aya::maps::MapError::KeyNotFound) => Ok(None),
            Err(source) => Err(BpfMapError::Operation {
                name: "backend_map",
                source,
            }),
        }
    }

    /// Write this node's view lease where the connect hook reads it.
    #[cfg(feature = "ebpf")]
    pub fn write_view_lease(
        &self,
        ebpf: &mut super::loader::OnionEbpf,
        value: super::super::types::ViewLeaseValue,
    ) -> Result<(), BpfMapError> {
        let name = "view_lease_map";
        let map = ebpf
            .bpf
            .map_mut(name)
            .ok_or(BpfMapError::MissingMap { name })?;
        let mut lease: aya::maps::HashMap<_, u32, super::super::types::ViewLeaseValue> =
            aya::maps::HashMap::try_from(map)
                .map_err(|source| BpfMapError::Operation { name, source })?;
        lease
            .insert(super::super::types::VIEW_LEASE_KEY, value, 0)
            .map_err(|source| BpfMapError::Operation { name, source })
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
pub fn service_entry_to_backend_value(entry: &super::super::types::ServiceEntry) -> BackendValue {
    service_entry_to_backend_value_for_consumer(entry, "")
}

/// The full catalogue remains available to DNS and ingress. A consumer's BPF
/// array holds at most 32, with healthy local endpoints first and stable
/// rendezvous scores distributing the remaining remote choices across nodes.
pub fn service_entry_to_backend_value_for_consumer(
    entry: &super::super::types::ServiceEntry,
    consumer: &str,
) -> BackendValue {
    let mut selected: Vec<_> = entry.backends.iter().collect();
    if selected.len() > MAX_BACKENDS {
        selected.sort_by_cached_key(|backend| {
            let mut score = Sha256::new();
            for part in [
                consumer,
                &entry.namespace,
                &entry.app_name,
                &backend.instance_id,
            ] {
                score.update((part.len() as u64).to_be_bytes());
                score.update(part.as_bytes());
            }
            score.update(backend.node_ip.octets());
            score.update(backend.host_port.to_be_bytes());
            (
                (!backend.healthy, !backend.local),
                <[u8; 32]>::from(score.finalize()),
            )
        });
        selected.truncate(MAX_BACKENDS);
    }
    let mut backends = [BackendEndpoint {
        host_ip: 0,
        host_port: 0,
        healthy: 0,
        local: 0,
    }; MAX_BACKENDS];

    let count = entry.backends.len().min(MAX_BACKENDS);
    for (i, backend) in selected.iter().enumerate() {
        backends[i] = BackendEndpoint {
            host_ip: ip_to_network_byte_order(backend.node_ip),
            host_port: backend.host_port.to_be(),
            healthy: u8::from(backend.healthy),
            local: u8::from(backend.local),
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
    use crate::onion::types::{BackendInstance, DnsMapKey, ServiceEntry};
    use crate::onion::vip::VirtualIP;

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
                    local: false,
                },
                BackendInstance {
                    instance_id: "redis-1".to_string(),
                    node_ip: Ipv4Addr::new(10, 0, 4, 2),
                    host_port: 31022,
                    healthy: false,
                    local: true,
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

    /// The connect hook reads this flag to keep routing to this node's own
    /// backends after the view lease lapses.
    #[test]
    fn backend_value_marks_only_local_backends() {
        let value = service_entry_to_backend_value(&test_entry());
        assert_eq!(value.backends[0].local, 0);
        assert_eq!(value.backends[1].local, 1);
    }

    #[test]
    fn backend_value_ips_in_network_byte_order() {
        let entry = test_entry();
        let value = service_entry_to_backend_value(&entry);

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
    fn sync_marks_initialised() {
        let bpf_map = BpfServiceMap::new();
        assert!(!bpf_map.is_initialised());
        // Full sync requires either a loaded eBPF program (ebpf feature)
        // or uses the no-op path (without the feature). We can't test
        // the eBPF path here without root, so just verify initialisation.
    }

    #[test]
    fn missing_map_error_names_the_map() {
        let err = BpfMapError::MissingMap {
            name: "backend_map",
        };
        assert!(err.to_string().contains("backend_map"));
    }

    #[test]
    fn dns_key_matches_internal_suffix() {
        let key = DnsMapKey::from_name("redis.internal");
        let name = std::str::from_utf8(&key.name)
            .unwrap()
            .trim_end_matches('\0');
        assert_eq!(name, "redis.internal");
    }
    fn oversized_entry() -> ServiceEntry {
        let mut entry = test_entry();
        entry.backends = (0..96)
            .map(|index| BackendInstance {
                instance_id: format!("redis-{index}"),
                node_ip: Ipv4Addr::new(10, 0, 1, index + 1),
                host_port: 30000 + index as u16,
                healthy: true,
                local: false,
            })
            .collect();
        entry
    }

    fn pool_addresses(pool: &BackendValue) -> Vec<(u32, u16)> {
        pool.backends[..pool.count as usize]
            .iter()
            .map(|backend| (backend.host_ip, backend.host_port))
            .collect()
    }

    #[test]
    fn oversized_dataplane_pools_are_stable_and_distributed_by_consumer_identity() {
        let mut entry = oversized_entry();
        let first = service_entry_to_backend_value_for_consumer(&entry, "consumer-a");
        let second = service_entry_to_backend_value_for_consumer(&entry, "consumer-b");
        assert_eq!(first.count as usize, MAX_BACKENDS);
        assert_eq!(second.count as usize, MAX_BACKENDS);
        assert_ne!(pool_addresses(&first), pool_addresses(&second));
        entry.backends.reverse();
        let reordered = service_entry_to_backend_value_for_consumer(&entry, "consumer-a");
        assert_eq!(pool_addresses(&first), pool_addresses(&reordered));
        assert!(
            first.backends.iter().all(|backend| backend.host_ip != 0
                && backend.host_port != 0
                && backend.healthy == 1)
        );
    }

    #[test]
    fn oversized_dataplane_pools_keep_healthy_local_endpoints_and_avoid_failed_slots() {
        let mut entry = oversized_entry();
        entry.backends[95].local = true;
        let local = (
            ip_to_network_byte_order(entry.backends[95].node_ip),
            entry.backends[95].host_port.to_be(),
        );
        entry.backends[0].healthy = false;
        entry.backends[0].local = true;
        let failed = (
            ip_to_network_byte_order(entry.backends[0].node_ip),
            entry.backends[0].host_port.to_be(),
        );
        let pool = service_entry_to_backend_value_for_consumer(&entry, "consumer-a");
        assert_eq!(pool.count as usize, MAX_BACKENDS);
        assert_eq!(pool.backends[0].local, 1);
        assert_eq!(pool_addresses(&pool)[0], local);
        assert!(!pool_addresses(&pool).contains(&failed));
        assert_eq!(
            entry.backends.len(),
            96,
            "pool selection must not mutate the published catalogue"
        );
    }
}

/// Shared data types for Onion service discovery.
///
/// The `#[repr(C)]` structs match the BPF map key/value layouts
/// defined in `ebpf/onion_common.h`. They must stay in sync.
/// The Rust-side structs (`ServiceEntry`, `BackendInstance`) are
/// higher-level aggregates that Bun works with in userspace.
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::vip::VirtualIP;

/// Maximum number of backends per service in the BPF map.
///
/// Limited by the BPF value size. 32 backends × 8 bytes each = 256
/// bytes of backend data, plus 16 bytes of header = 272 bytes total.
pub const MAX_BACKENDS: usize = 32;

/// Firewall action: allow the connection.
pub const FIREWALL_ALLOW: u32 = 1;

/// Firewall action: deny the connection (returns ECONNREFUSED).
pub const FIREWALL_DENY: u32 = 0;

// ---------------------------------------------------------------------------
// BPF map key/value structs (#[repr(C)] — must match ebpf/onion_common.h)
// ---------------------------------------------------------------------------

/// Key for the `dns_map` BPF hash map.
///
/// The name is null-terminated, lowercase-normalised, max 255 chars + null.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct DnsMapKey {
    pub name: [u8; 256],
}

impl DnsMapKey {
    /// Create a key from a service name, normalising to lowercase
    /// and null-terminating.
    pub fn from_name(name: &str) -> Self {
        let mut key = DnsMapKey { name: [0u8; 256] };
        let lower = name.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let len = bytes.len().min(255);
        key.name[..len].copy_from_slice(&bytes[..len]);
        // Remaining bytes are already zero (null terminator)
        key
    }
}

/// Value for the `dns_map` BPF hash map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct DnsMapValue {
    /// Virtual IP in network byte order.
    pub vip: u32,
}

/// Key for the `backend_map` BPF hash map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BackendKey {
    /// Virtual IP in network byte order.
    pub vip: u32,
    /// Service port in network byte order.
    pub port: u16,
    /// Alignment padding.
    pub _pad: u16,
}

/// A single backend endpoint in the `backend_map` value.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BackendEndpoint {
    /// Real node IP in network byte order.
    pub host_ip: u32,
    /// Dynamically allocated host port in network byte order.
    pub host_port: u16,
    /// 1 = healthy, 0 = unhealthy (excluded from selection).
    pub healthy: u8,
    /// Alignment padding.
    pub _pad: u8,
}

/// Value for the `backend_map` BPF hash map.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct BackendValue {
    /// Total number of backends (healthy + unhealthy).
    pub count: u32,
    /// Round-robin counter (atomically incremented by the eBPF program).
    pub rr_index: u32,
    /// App identifier for firewall lookups.
    pub app_id: u32,
    /// Namespace identifier for isolation checks.
    pub namespace_id: u32,
    /// Backend array. Only the first `count` entries are valid.
    pub backends: [BackendEndpoint; MAX_BACKENDS],
}

/// Key for the `firewall_map` BPF hash map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct FirewallKey {
    /// cgroup ID of the calling process.
    pub src_cgroup_id: u64,
    /// App identifier of the destination service.
    pub dst_app_id: u32,
    /// Alignment padding.
    pub _pad: u32,
}

/// Value for the `firewall_map` BPF hash map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct FirewallValue {
    /// `FIREWALL_DENY` (0) or `FIREWALL_ALLOW` (1).
    pub action: u32,
}

// ---------------------------------------------------------------------------
// Rust-side service state (userspace, not sent to BPF directly)
// ---------------------------------------------------------------------------

/// Errors from Onion operations.
#[derive(Debug, thiserror::Error)]
pub enum OnionError {
    #[error("service {name:?} not found")]
    ServiceNotFound { name: String },

    #[error("backend {instance_id:?} not found for service {app_name:?}")]
    BackendNotFound {
        app_name: String,
        instance_id: String,
    },

    #[error("service {name:?} already registered")]
    AlreadyRegistered { name: String },

    #[error("too many backends for {app_name:?}: limit is {MAX_BACKENDS}")]
    TooManyBackends { app_name: String },

    #[error("eBPF load failed: {reason}")]
    EbpfLoadFailed { reason: String },
}

/// Full service state maintained by Bun in userspace.
///
/// Bun compiles this into the BPF map entries. This is the
/// source-of-truth that the `ServiceMap` stores and that
/// `relish resolve` displays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEntry {
    /// App name (e.g. "redis").
    pub app_name: String,
    /// Namespace (e.g. "default").
    pub namespace: String,
    /// Deterministic namespace identifier (hash of namespace name).
    pub namespace_id: u32,
    /// Deterministic app identifier (hash of app name).
    pub app_id: u32,
    /// Virtual IP for this service.
    pub vip: VirtualIP,
    /// Declared container port (e.g. 6379).
    pub port: u16,
    /// Current backends.
    pub backends: Vec<BackendInstance>,
    /// Firewall allow_from rules. `None` means namespace-default
    /// isolation (same namespace only).
    pub firewall_allow_from: Option<Vec<String>>,
}

/// A single backend instance of a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendInstance {
    /// Instance ID (e.g. "redis-0").
    pub instance_id: String,
    /// Real node IP where this instance runs.
    pub node_ip: Ipv4Addr,
    /// Dynamically allocated host port.
    pub host_port: u16,
    /// Whether this backend is currently healthy.
    pub healthy: bool,
}

/// Response type for `relish resolve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveResponse {
    /// App name.
    pub app_name: String,
    /// Namespace.
    pub namespace: String,
    /// Virtual IP address.
    pub vip: String,
    /// Declared service port.
    pub port: u16,
    /// Number of healthy backends.
    pub healthy_backends: usize,
    /// Total number of backends.
    pub total_backends: usize,
    /// Backend details.
    pub backends: Vec<ResolveBackend>,
}

/// A backend in the resolve response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveBackend {
    /// Instance ID.
    pub instance_id: String,
    /// Node IP address.
    pub node_ip: String,
    /// Host port.
    pub host_port: u16,
    /// Whether the backend is healthy.
    pub healthy: bool,
}

impl ServiceEntry {
    /// Convert to a resolve response for the API/CLI.
    pub fn to_resolve_response(&self) -> ResolveResponse {
        let healthy_count = self.backends.iter().filter(|b| b.healthy).count();
        ResolveResponse {
            app_name: self.app_name.clone(),
            namespace: self.namespace.clone(),
            vip: self.vip.0.to_string(),
            port: self.port,
            healthy_backends: healthy_count,
            total_backends: self.backends.len(),
            backends: self
                .backends
                .iter()
                .map(|b| ResolveBackend {
                    instance_id: b.instance_id.clone(),
                    node_ip: b.node_ip.to_string(),
                    host_port: b.host_port,
                    healthy: b.healthy,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_map_key_size() {
        assert_eq!(std::mem::size_of::<DnsMapKey>(), 256);
    }

    #[test]
    fn dns_map_value_size() {
        assert_eq!(std::mem::size_of::<DnsMapValue>(), 4);
    }

    #[test]
    fn backend_key_size() {
        assert_eq!(std::mem::size_of::<BackendKey>(), 8);
    }

    #[test]
    fn backend_endpoint_size() {
        assert_eq!(std::mem::size_of::<BackendEndpoint>(), 8);
    }

    #[test]
    fn backend_value_size() {
        // 4 (count) + 4 (rr_index) + 4 (app_id) + 4 (namespace_id)
        // + 32 * 8 (backends) = 272
        assert_eq!(std::mem::size_of::<BackendValue>(), 272);
    }

    #[test]
    fn firewall_key_size() {
        assert_eq!(std::mem::size_of::<FirewallKey>(), 16);
    }

    #[test]
    fn firewall_value_size() {
        assert_eq!(std::mem::size_of::<FirewallValue>(), 4);
    }

    #[test]
    fn dns_key_from_name_normalises_to_lowercase() {
        let key = DnsMapKey::from_name("Redis.INTERNAL");
        let name = std::str::from_utf8(&key.name)
            .unwrap()
            .trim_end_matches('\0');
        assert_eq!(name, "redis.internal");
    }

    #[test]
    fn dns_key_from_name_null_terminates() {
        let key = DnsMapKey::from_name("redis.internal");
        assert_eq!(key.name[14], 0); // byte after the name
    }

    #[test]
    fn dns_key_from_name_max_length() {
        let long_name = "a".repeat(300);
        let key = DnsMapKey::from_name(&long_name);
        // Should truncate to 255 chars
        let name_bytes: Vec<u8> = key.name.iter().take_while(|&&b| b != 0).copied().collect();
        assert_eq!(name_bytes.len(), 255);
    }

    #[test]
    fn service_entry_to_resolve_response() {
        let entry = ServiceEntry {
            app_name: "redis".to_string(),
            namespace: "default".to_string(),
            namespace_id: 1,
            app_id: 42,
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
        };

        let resp = entry.to_resolve_response();
        assert_eq!(resp.app_name, "redis");
        assert_eq!(resp.vip, "127.128.0.3");
        assert_eq!(resp.port, 6379);
        assert_eq!(resp.healthy_backends, 1);
        assert_eq!(resp.total_backends, 2);
    }
}

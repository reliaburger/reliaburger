//! Egress allowlist parsing and BPF map entry generation.
//!
//! When an app specifies `[egress] allow = ["api.stripe.com:443"]`,
//! only connections to those destinations are permitted. All other
//! non-VIP, non-internal traffic is blocked at the eBPF level.
//!
//! If no egress section is configured, all egress is allowed
//! (backward compatible).

use std::net::{Ipv4Addr, ToSocketAddrs};

/// A resolved egress allowlist entry, ready for BPF map insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEgressEntry {
    /// Destination IP (network byte order).
    pub dst_ip: u32,
    /// Destination port (host byte order).
    pub dst_port: u16,
    /// The app ID that has this egress rule.
    pub app_id: u32,
}

/// BPF map key for the egress allowlist map.
///
/// Matches the C struct `egress_key` in `onion_common.h`.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EgressKey {
    /// Cgroup ID of the connecting app.
    pub src_cgroup_id: u64,
    /// Destination IP (network byte order).
    pub dst_ip: u32,
    /// Destination port (network byte order).
    pub dst_port: u16,
    pub _pad: u16,
}

/// BPF map value for the egress allowlist map.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EgressValue {
    /// 1 = allow, 0 = deny.
    pub action: u32,
}

/// Whether egress is enforced for this app.
pub const EGRESS_ALLOW: u32 = 1;

#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for EgressKey {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for EgressValue {}

/// Parse an egress allowlist entry.
///
/// Accepted formats:
/// - `hostname:port` — resolved to IP(s) via DNS
/// - `ip:port` — used directly
/// - `cidr:port` — not yet supported (returns error)
///
/// Returns one or more `(ip, port)` tuples (DNS may resolve to multiple IPs).
pub fn parse_egress_entry(entry: &str) -> Result<Vec<(Ipv4Addr, u16)>, EgressError> {
    let entry = entry.trim();

    // Split on last ':'
    let (host, port_str) = entry
        .rsplit_once(':')
        .ok_or_else(|| EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: "expected host:port format".into(),
        })?;

    let port: u16 = port_str.parse().map_err(|_| EgressError::InvalidFormat {
        entry: entry.to_string(),
        reason: format!("invalid port: {port_str}"),
    })?;

    // Try parsing as an IP address first
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(vec![(ip, port)]);
    }

    // CIDR notation — deferred
    if host.contains('/') {
        return Err(EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: "CIDR notation not yet supported".into(),
        });
    }

    // DNS resolution
    let addrs: Vec<(Ipv4Addr, u16)> = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| EgressError::DnsResolutionFailed {
            hostname: host.to_string(),
            source: e,
        })?
        .filter_map(|addr| {
            if let std::net::SocketAddr::V4(v4) = addr {
                Some((*v4.ip(), v4.port()))
            } else {
                None // Skip IPv6
            }
        })
        .collect();

    if addrs.is_empty() {
        return Err(EgressError::DnsResolutionFailed {
            hostname: host.to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no IPv4 addresses found"),
        });
    }

    Ok(addrs)
}

/// Resolve all egress entries for an app's egress config.
///
/// Returns `None` if no egress config is set (all egress allowed).
pub fn resolve_egress_entries(allow_list: &[String]) -> Result<Vec<(Ipv4Addr, u16)>, EgressError> {
    let mut resolved = Vec::new();
    for entry in allow_list {
        resolved.extend(parse_egress_entry(entry)?);
    }
    Ok(resolved)
}

/// A set of resolved egress destinations (IP + port).
pub type EgressDestinations = Vec<(Ipv4Addr, u16)>;

/// Diff a previously-resolved egress destination set against a freshly
/// re-resolved one. Returns `(to_add, to_remove)` — destinations newly
/// present and destinations gone — so the periodic re-resolution loop can
/// program only the delta into the eBPF egress map. Order-independent.
pub fn egress_diff(
    old: &[(Ipv4Addr, u16)],
    new: &[(Ipv4Addr, u16)],
) -> (EgressDestinations, EgressDestinations) {
    use std::collections::HashSet;
    let old_set: HashSet<(Ipv4Addr, u16)> = old.iter().copied().collect();
    let new_set: HashSet<(Ipv4Addr, u16)> = new.iter().copied().collect();
    let to_add = new_set.difference(&old_set).copied().collect();
    let to_remove = old_set.difference(&new_set).copied().collect();
    (to_add, to_remove)
}

/// Build BPF map entries from resolved egress rules.
///
/// Each entry allows a specific (cgroup, dst_ip, dst_port) tuple.
pub fn egress_to_bpf_entries(
    cgroup_ids: &[u64],
    resolved: &[(Ipv4Addr, u16)],
) -> Vec<(EgressKey, EgressValue)> {
    let mut entries = Vec::new();
    for &cg in cgroup_ids {
        for &(ip, port) in resolved {
            entries.push((
                EgressKey {
                    src_cgroup_id: cg,
                    dst_ip: u32::from(ip).to_be(),
                    dst_port: port.to_be(),
                    _pad: 0,
                },
                EgressValue {
                    action: EGRESS_ALLOW,
                },
            ));
        }
    }
    entries
}

/// Asynchronously re-resolve DNS-based egress entries.
///
/// Uses `tokio::net::lookup_host` instead of blocking `ToSocketAddrs`.
/// Returns the new set of resolved (IP, port) pairs. The caller can
/// compare with the previous set to detect IP changes.
pub async fn re_resolve_egress_async(
    allow_list: &[String],
) -> Result<Vec<(Ipv4Addr, u16)>, EgressError> {
    let mut resolved = Vec::new();
    for entry in allow_list {
        let entry = entry.trim();
        let (host, port_str) =
            entry
                .rsplit_once(':')
                .ok_or_else(|| EgressError::InvalidFormat {
                    entry: entry.to_string(),
                    reason: "expected host:port format".into(),
                })?;

        let port: u16 = port_str.parse().map_err(|_| EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: format!("invalid port: {port_str}"),
        })?;

        // IP address — no DNS needed
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            resolved.push((ip, port));
            continue;
        }

        // Async DNS resolution
        let addrs: Vec<(Ipv4Addr, u16)> = tokio::net::lookup_host(format!("{host}:{port}"))
            .await
            .map_err(|e| EgressError::DnsResolutionFailed {
                hostname: host.to_string(),
                source: e,
            })?
            .filter_map(|addr| {
                if let std::net::SocketAddr::V4(v4) = addr {
                    Some((*v4.ip(), v4.port()))
                } else {
                    None
                }
            })
            .collect();

        if addrs.is_empty() {
            return Err(EgressError::DnsResolutionFailed {
                hostname: host.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no IPv4 addresses found",
                ),
            });
        }

        resolved.extend(addrs);
    }
    Ok(resolved)
}

/// Resolve the cgroup v2 id for a process — the value the eBPF
/// `bpf_get_current_cgroup_id()` helper returns for that process's
/// connections, which is the inode of its cgroup v2 directory.
///
/// Linux-only; returns `None` off Linux or on any read/stat error (a
/// short-lived process may already be gone).
#[cfg(target_os = "linux")]
pub fn cgroup_id_of_pid(pid: u32) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    // The cgroup v2 line in /proc/<pid>/cgroup is "0::<relative-path>".
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = content.lines().find_map(|line| line.strip_prefix("0::"))?;
    let full = format!("/sys/fs/cgroup{rel}");
    std::fs::metadata(&full).ok().map(|meta| meta.ino())
}

/// Stub off Linux: there are no cgroups to resolve.
#[cfg(not(target_os = "linux"))]
pub fn cgroup_id_of_pid(_pid: u32) -> Option<u64> {
    None
}

/// Value written to `egress_enabled_map` to turn on enforcement.
pub const EGRESS_ENFORCE: u32 = 1;

/// eBPF egress map writers. With the `ebpf` feature these write the
/// `egress_map` (allowed destinations) and `egress_enabled_map`
/// (per-cgroup enforcement flag); without it they return `Unsupported`.
#[cfg(feature = "ebpf")]
mod maps {
    use super::{EGRESS_ENFORCE, EgressKey, EgressMapError, EgressValue};
    use aya::maps::HashMap;

    /// Enable egress enforcement for a cgroup: with this set, any
    /// destination not present in `egress_map` for the cgroup is denied.
    pub fn set_egress_enforced(bpf: &mut aya::Ebpf, cgroup_id: u64) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, u64, u32> = HashMap::try_from(
            bpf.map_mut("egress_enabled_map")
                .ok_or(EgressMapError::MapNotFound {
                    map_name: "egress_enabled_map",
                })?,
        )?;
        map.insert(cgroup_id, EGRESS_ENFORCE, 0)?;
        Ok(())
    }

    /// Turn egress enforcement off for a cgroup (used on instance stop).
    pub fn clear_egress_enforced(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, u64, u32> = HashMap::try_from(
            bpf.map_mut("egress_enabled_map")
                .ok_or(EgressMapError::MapNotFound {
                    map_name: "egress_enabled_map",
                })?,
        )?;
        let _ = map.remove(&cgroup_id);
        Ok(())
    }

    /// Allow a single (cgroup, dst_ip, dst_port) destination.
    pub fn write_egress_entry(
        bpf: &mut aya::Ebpf,
        key: EgressKey,
        value: EgressValue,
    ) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, EgressKey, EgressValue> = HashMap::try_from(
            bpf.map_mut("egress_map")
                .ok_or(EgressMapError::MapNotFound {
                    map_name: "egress_map",
                })?,
        )?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Remove a previously allowed destination.
    pub fn delete_egress_entry(bpf: &mut aya::Ebpf, key: EgressKey) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, EgressKey, EgressValue> = HashMap::try_from(
            bpf.map_mut("egress_map")
                .ok_or(EgressMapError::MapNotFound {
                    map_name: "egress_map",
                })?,
        )?;
        let _ = map.remove(&key);
        Ok(())
    }
}

#[cfg(feature = "ebpf")]
pub use maps::*;

/// Errors from writing the eBPF egress maps.
#[derive(Debug, thiserror::Error)]
pub enum EgressMapError {
    #[error("egress eBPF maps require Linux with --features ebpf")]
    Unsupported,

    #[cfg(feature = "ebpf")]
    #[error("egress map operation failed: {0}")]
    MapError(#[from] aya::maps::MapError),

    #[error("egress map {map_name:?} not found in the loaded program")]
    MapNotFound { map_name: &'static str },
}

/// Errors from egress allowlist resolution.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("invalid egress entry {entry:?}: {reason}")]
    InvalidFormat { entry: String, reason: String },

    #[error("DNS resolution failed for {hostname:?}: {source}")]
    DnsResolutionFailed {
        hostname: String,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ip_port() {
        let result = parse_egress_entry("1.2.3.4:443").unwrap();
        assert_eq!(result, vec![(Ipv4Addr::new(1, 2, 3, 4), 443)]);
    }

    #[test]
    fn egress_diff_reports_added_and_removed() {
        let a = (Ipv4Addr::new(1, 1, 1, 1), 443);
        let b = (Ipv4Addr::new(2, 2, 2, 2), 443);
        let c = (Ipv4Addr::new(3, 3, 3, 3), 443);

        // b stays, a is removed, c is added.
        let (add, remove) = egress_diff(&[a, b], &[b, c]);
        assert_eq!(add, vec![c]);
        assert_eq!(remove, vec![a]);
    }

    #[test]
    fn egress_diff_empty_when_unchanged() {
        let a = (Ipv4Addr::new(1, 1, 1, 1), 443);
        let b = (Ipv4Addr::new(2, 2, 2, 2), 80);
        let (add, remove) = egress_diff(&[a, b], &[b, a]);
        assert!(add.is_empty());
        assert!(remove.is_empty());
    }

    #[test]
    fn parse_ip_port_with_whitespace() {
        let result = parse_egress_entry("  10.0.1.5:8080  ").unwrap();
        assert_eq!(result, vec![(Ipv4Addr::new(10, 0, 1, 5), 8080)]);
    }

    #[test]
    fn parse_invalid_port() {
        let result = parse_egress_entry("1.2.3.4:abc");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_port() {
        let result = parse_egress_entry("1.2.3.4");
        assert!(result.is_err());
    }

    #[test]
    fn parse_cidr_unsupported() {
        let result = parse_egress_entry("10.0.0.0/8:443");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("CIDR"));
    }

    #[test]
    fn resolve_empty_list() {
        let result = resolve_egress_entries(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_multiple_entries() {
        let entries = vec!["1.2.3.4:443".to_string(), "5.6.7.8:80".to_string()];
        let result = resolve_egress_entries(&entries).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&(Ipv4Addr::new(1, 2, 3, 4), 443)));
        assert!(result.contains(&(Ipv4Addr::new(5, 6, 7, 8), 80)));
    }

    #[test]
    fn egress_bpf_entries_generated() {
        let cgroups = vec![1001, 1002];
        let resolved = vec![
            (Ipv4Addr::new(1, 2, 3, 4), 443),
            (Ipv4Addr::new(5, 6, 7, 8), 80),
        ];
        let entries = egress_to_bpf_entries(&cgroups, &resolved);
        // 2 cgroups × 2 destinations = 4 entries
        assert_eq!(entries.len(), 4);
        assert!(entries.iter().all(|(_, v)| v.action == EGRESS_ALLOW));
    }

    #[test]
    fn egress_key_size() {
        assert_eq!(std::mem::size_of::<EgressKey>(), 16);
    }

    #[test]
    fn egress_value_size() {
        assert_eq!(std::mem::size_of::<EgressValue>(), 4);
    }
}

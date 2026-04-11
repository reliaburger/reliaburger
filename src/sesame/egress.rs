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

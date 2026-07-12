//! Egress allowlist parsing and BPF map entry generation.
//!
//! When an app specifies `[egress] allow = ["api.stripe.com:443"]`,
//! only connections to those destinations are permitted. All other
//! non-VIP, non-internal traffic is blocked at the eBPF level.
//!
//! Accepted entry forms (NET7):
//! - `1.2.3.4:443` — exact IPv4 destination
//! - `10.0.0.0/8:443` — IPv4 CIDR (enforced via an LPM trie)
//! - `[2001:db8::1]:443` — exact IPv6 destination (brackets required)
//! - `[2001:db8::/32]:443` — IPv6 CIDR
//! - `hostname:443` — resolved via DNS, keeping both A and AAAA records
//!
//! If no egress section is configured, all egress is allowed
//! (backward compatible).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// A single resolved egress destination, ready to program into the
/// kernel maps. Addresses and ports are in host byte order; the map
/// entry builders below convert to network byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EgressDestination {
    /// An exact address and port.
    Ip { ip: IpAddr, port: u16 },
    /// A CIDR network and port. The network address is normalised
    /// (no host bits) — the parser rejects anything else.
    Cidr {
        network: IpAddr,
        prefix_len: u8,
        port: u16,
    },
}

/// BPF map key for the IPv4 egress allowlist map.
///
/// Matches the C struct `egress_key` in `onion_common.h`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EgressKey {
    /// Cgroup ID of the connecting app.
    pub src_cgroup_id: u64,
    /// Destination IP (network byte order).
    pub dst_ip: u32,
    /// Destination port (network byte order).
    pub dst_port: u16,
    pub _pad: u16,
}

/// BPF map key for the IPv6 egress allowlist map.
///
/// Matches the C struct `egress6_key` in `onion_common.h`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Egress6Key {
    /// Cgroup ID of the connecting app.
    pub src_cgroup_id: u64,
    /// Destination IPv6 address (network byte order).
    pub dst_ip: [u8; 16],
    /// Destination port (network byte order).
    pub dst_port: u16,
    pub _pad: u16,
    pub _pad2: u32,
}

/// BPF map value for the egress allowlist maps.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EgressValue {
    /// 1 = allow, 0 = deny.
    pub action: u32,
}

/// Maximum distinct ports per CIDR entry — the fixed-size port array in
/// the LPM trie value (`egress_cidr_value` in `onion_common.h`).
pub const MAX_CIDR_PORTS: usize = 8;

/// The cgroup id occupies the first 64 bits of every LPM trie key, and
/// is always fully matched.
pub const CIDR_CGROUP_PREFIX_BITS: u32 = 64;

/// BPF map value for the CIDR allowlist LPM tries.
///
/// Matches the C struct `egress_cidr_value` in `onion_common.h`.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EgressCidrValue {
    /// Allowed ports (network byte order).
    pub ports: [u16; MAX_CIDR_PORTS],
    /// How many entries of `ports` are valid.
    pub count: u16,
    pub _pad: u16,
}

/// Whether egress is enforced for this app.
pub const EGRESS_ALLOW: u32 = 1;

#[cfg(feature = "ebpf")]
// SAFETY: plain-old-data structs — #[repr(C)], no padding surprises
// (explicit _pad fields), every bit pattern is a valid value.
unsafe impl aya::Pod for EgressKey {}
#[cfg(feature = "ebpf")]
// SAFETY: as above.
unsafe impl aya::Pod for Egress6Key {}
#[cfg(feature = "ebpf")]
// SAFETY: as above.
unsafe impl aya::Pod for EgressValue {}
#[cfg(feature = "ebpf")]
// SAFETY: as above.
unsafe impl aya::Pod for EgressCidrValue {}

/// Build the exact-match IPv4 map key for a destination.
pub fn exact_v4_key(cgroup_id: u64, ip: Ipv4Addr, port: u16) -> EgressKey {
    EgressKey {
        src_cgroup_id: cgroup_id,
        dst_ip: u32::from(ip).to_be(),
        dst_port: port.to_be(),
        _pad: 0,
    }
}

/// Build the exact-match IPv6 map key for a destination.
pub fn exact_v6_key(cgroup_id: u64, ip: Ipv6Addr, port: u16) -> Egress6Key {
    Egress6Key {
        src_cgroup_id: cgroup_id,
        dst_ip: ip.octets(),
        dst_port: port.to_be(),
        _pad: 0,
        _pad2: 0,
    }
}

/// LPM trie key data for an IPv4 CIDR: cgroup id (big-endian, so both
/// sides of the trie produce identical bytes) followed by the network
/// address in network byte order.
pub fn cidr4_lpm_data(cgroup_id: u64, network: Ipv4Addr) -> [u8; 12] {
    let mut data = [0u8; 12];
    data[..8].copy_from_slice(&cgroup_id.to_be_bytes());
    data[8..].copy_from_slice(&network.octets());
    data
}

/// LPM trie key data for an IPv6 CIDR (see `cidr4_lpm_data`).
pub fn cidr6_lpm_data(cgroup_id: u64, network: Ipv6Addr) -> [u8; 24] {
    let mut data = [0u8; 24];
    data[..8].copy_from_slice(&cgroup_id.to_be_bytes());
    data[8..].copy_from_slice(&network.octets());
    data
}

/// Build the LPM trie value from a set of allowed ports (host order).
/// The caller guarantees `ports.len() <= MAX_CIDR_PORTS` (enforced by
/// `merge_cidr_ports`).
pub fn cidr_value(ports: &[u16]) -> EgressCidrValue {
    let mut value = EgressCidrValue {
        ports: [0; MAX_CIDR_PORTS],
        count: ports.len().min(MAX_CIDR_PORTS) as u16,
        _pad: 0,
    };
    for (i, port) in ports.iter().take(MAX_CIDR_PORTS).enumerate() {
        value.ports[i] = port.to_be();
    }
    value
}

/// A CIDR allow entry after port merging: one LPM trie entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CidrAllow {
    /// The network address (normalised).
    pub network: IpAddr,
    /// Prefix length in bits.
    pub prefix_len: u8,
    /// Allowed ports (host order, sorted, deduplicated).
    pub ports: Vec<u16>,
}

/// Does `outer` (network, prefix) contain `inner`? Same-family only.
fn cidr_contains(outer_net: IpAddr, outer_len: u8, inner_net: IpAddr, inner_len: u8) -> bool {
    if outer_len > inner_len {
        return false;
    }
    match (outer_net, inner_net) {
        (IpAddr::V4(o), IpAddr::V4(i)) => {
            let mask = v4_mask(outer_len);
            u32::from(i) & mask == u32::from(o)
        }
        (IpAddr::V6(o), IpAddr::V6(i)) => {
            let mask = v6_mask(outer_len);
            u128::from(i) & mask == u128::from(o)
        }
        _ => false,
    }
}

fn v4_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    }
}

fn v6_mask(prefix_len: u8) -> u128 {
    if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix_len))
    }
}

/// Fold a destination set's CIDR entries into per-prefix LPM values.
///
/// An LPM lookup returns only the *longest* matching prefix, so a nested
/// CIDR would otherwise shadow an enclosing one: with `10.0.0.0/8:443`
/// and `10.1.0.0/16:80` configured, a connect to `10.1.2.3:443` matches
/// the /16 entry and would be denied. We therefore fold the ports of
/// every enclosing prefix into each more-specific entry.
///
/// Errors when a single prefix ends up with more than `MAX_CIDR_PORTS`
/// ports — the kernel value has a fixed-size port array.
pub fn merge_cidr_ports(dests: &[EgressDestination]) -> Result<Vec<CidrAllow>, EgressError> {
    use std::collections::BTreeMap;

    // Group ports by (network, prefix_len). BTreeMap for deterministic order.
    let mut groups: BTreeMap<(IpAddr, u8), Vec<u16>> = BTreeMap::new();
    for dest in dests {
        if let EgressDestination::Cidr {
            network,
            prefix_len,
            port,
        } = dest
        {
            groups
                .entry((*network, *prefix_len))
                .or_default()
                .push(*port);
        }
    }

    let keys: Vec<(IpAddr, u8)> = groups.keys().copied().collect();
    let mut merged = Vec::new();
    for (network, prefix_len) in &keys {
        let mut ports: Vec<u16> = groups[&(*network, *prefix_len)].clone();
        // Fold in the ports of every *other* prefix that contains this one.
        for (other_net, other_len) in &keys {
            if (other_net, other_len) == (network, prefix_len) {
                continue;
            }
            if cidr_contains(*other_net, *other_len, *network, *prefix_len) {
                ports.extend(&groups[&(*other_net, *other_len)]);
            }
        }
        ports.sort_unstable();
        ports.dedup();
        if ports.len() > MAX_CIDR_PORTS {
            return Err(EgressError::TooManyCidrPorts {
                network: format!("{network}/{prefix_len}"),
                count: ports.len(),
                limit: MAX_CIDR_PORTS,
            });
        }
        merged.push(CidrAllow {
            network: *network,
            prefix_len: *prefix_len,
            ports,
        });
    }
    Ok(merged)
}

/// A statically-parseable entry, or one that needs DNS resolution.
enum StaticParse {
    Resolved(Vec<EgressDestination>),
    NeedsDns { host: String, port: u16 },
}

/// Shared front half of sync and async parsing: everything except the
/// actual DNS lookup.
fn parse_static(entry: &str) -> Result<StaticParse, EgressError> {
    let entry = entry.trim();

    // Bracketed IPv6: "[addr]:port" or "[addr/prefix]:port".
    if let Some(rest) = entry.strip_prefix('[') {
        let (inside, after) = rest
            .split_once(']')
            .ok_or_else(|| EgressError::InvalidFormat {
                entry: entry.to_string(),
                reason: "unterminated '[' in IPv6 entry".into(),
            })?;
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| EgressError::InvalidFormat {
                entry: entry.to_string(),
                reason: "expected [address]:port format".into(),
            })?;
        let port = parse_port(entry, port_str)?;

        if let Some((net_str, len_str)) = inside.split_once('/') {
            let network: Ipv6Addr = net_str.parse().map_err(|_| EgressError::InvalidFormat {
                entry: entry.to_string(),
                reason: format!("invalid IPv6 network address: {net_str}"),
            })?;
            let prefix_len = parse_prefix(entry, len_str, 128)?;
            let mask = v6_mask(prefix_len);
            if u128::from(network) & !mask != 0 {
                let normalised = Ipv6Addr::from(u128::from(network) & mask);
                return Err(EgressError::InvalidFormat {
                    entry: entry.to_string(),
                    reason: format!(
                        "host bits set in CIDR; did you mean [{normalised}/{prefix_len}]"
                    ),
                });
            }
            return Ok(StaticParse::Resolved(vec![EgressDestination::Cidr {
                network: IpAddr::V6(network),
                prefix_len,
                port,
            }]));
        }

        let ip: Ipv6Addr = inside.parse().map_err(|_| EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: format!("invalid IPv6 address: {inside}"),
        })?;
        return Ok(StaticParse::Resolved(vec![EgressDestination::Ip {
            ip: IpAddr::V6(ip),
            port,
        }]));
    }

    // Split on last ':'.
    let (host, port_str) = entry
        .rsplit_once(':')
        .ok_or_else(|| EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: "expected host:port format".into(),
        })?;
    let port = parse_port(entry, port_str)?;

    // Exact IPv4.
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(StaticParse::Resolved(vec![EgressDestination::Ip {
            ip: IpAddr::V4(ip),
            port,
        }]));
    }

    // IPv4 CIDR.
    if let Some((net_str, len_str)) = host.split_once('/') {
        let network: Ipv4Addr = net_str.parse().map_err(|_| EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: format!("invalid IPv4 network address: {net_str}"),
        })?;
        let prefix_len = parse_prefix(entry, len_str, 32)?;
        let mask = v4_mask(prefix_len);
        if u32::from(network) & !mask != 0 {
            let normalised = Ipv4Addr::from(u32::from(network) & mask);
            return Err(EgressError::InvalidFormat {
                entry: entry.to_string(),
                reason: format!("host bits set in CIDR; did you mean {normalised}/{prefix_len}"),
            });
        }
        return Ok(StaticParse::Resolved(vec![EgressDestination::Cidr {
            network: IpAddr::V4(network),
            prefix_len,
            port,
        }]));
    }

    // A bare IPv6 address without brackets is ambiguous (which ':' is
    // the port?) — require brackets rather than guess.
    if host.parse::<Ipv6Addr>().is_ok() {
        return Err(EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: "IPv6 addresses must be bracketed: [address]:port".into(),
        });
    }

    Ok(StaticParse::NeedsDns {
        host: host.to_string(),
        port,
    })
}

fn parse_port(entry: &str, port_str: &str) -> Result<u16, EgressError> {
    port_str.parse().map_err(|_| EgressError::InvalidFormat {
        entry: entry.to_string(),
        reason: format!("invalid port: {port_str}"),
    })
}

fn parse_prefix(entry: &str, len_str: &str, max: u8) -> Result<u8, EgressError> {
    let prefix_len: u8 = len_str.parse().map_err(|_| EgressError::InvalidFormat {
        entry: entry.to_string(),
        reason: format!("invalid prefix length: {len_str}"),
    })?;
    if prefix_len > max {
        return Err(EgressError::InvalidFormat {
            entry: entry.to_string(),
            reason: format!("prefix length {prefix_len} out of range (0-{max})"),
        });
    }
    Ok(prefix_len)
}

/// Parse an egress allowlist entry (see the module docs for the accepted
/// forms). Hostnames resolve to one destination per A/AAAA record.
pub fn parse_egress_entry(entry: &str) -> Result<Vec<EgressDestination>, EgressError> {
    match parse_static(entry)? {
        StaticParse::Resolved(dests) => Ok(dests),
        StaticParse::NeedsDns { host, port } => {
            let dests: Vec<EgressDestination> = format!("{host}:{port}")
                .to_socket_addrs()
                .map_err(|e| EgressError::DnsResolutionFailed {
                    hostname: host.clone(),
                    source: e,
                })?
                .map(|addr| EgressDestination::Ip {
                    ip: addr.ip(),
                    port: addr.port(),
                })
                .collect();
            if dests.is_empty() {
                return Err(EgressError::DnsResolutionFailed {
                    hostname: host,
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses found"),
                });
            }
            Ok(dests)
        }
    }
}

/// Resolve all egress entries for an app's egress config.
pub fn resolve_egress_entries(
    allow_list: &[String],
) -> Result<Vec<EgressDestination>, EgressError> {
    let mut resolved = Vec::new();
    for entry in allow_list {
        resolved.extend(parse_egress_entry(entry)?);
    }
    Ok(resolved)
}

/// A set of resolved egress destinations.
pub type EgressDestinations = Vec<EgressDestination>;

/// Diff a previously-resolved egress destination set against a freshly
/// re-resolved one. Returns `(to_add, to_remove)` — destinations newly
/// present and destinations gone — so the periodic re-resolution loop
/// can reprogram the eBPF egress maps. Order-independent.
pub fn egress_diff(
    old: &[EgressDestination],
    new: &[EgressDestination],
) -> (EgressDestinations, EgressDestinations) {
    let old_set: HashSet<EgressDestination> = old.iter().copied().collect();
    let new_set: HashSet<EgressDestination> = new.iter().copied().collect();
    let to_add = new_set.difference(&old_set).copied().collect();
    let to_remove = old_set.difference(&new_set).copied().collect();
    (to_add, to_remove)
}

/// Asynchronously re-resolve DNS-based egress entries.
///
/// Uses `tokio::net::lookup_host` instead of blocking `ToSocketAddrs`.
/// Returns the new set of resolved destinations. The caller can compare
/// with the previous set (`egress_diff`) to detect IP changes.
pub async fn re_resolve_egress_async(
    allow_list: &[String],
) -> Result<Vec<EgressDestination>, EgressError> {
    let mut resolved = Vec::new();
    for entry in allow_list {
        match parse_static(entry)? {
            StaticParse::Resolved(dests) => resolved.extend(dests),
            StaticParse::NeedsDns { host, port } => {
                let dests: Vec<EgressDestination> =
                    tokio::net::lookup_host(format!("{host}:{port}"))
                        .await
                        .map_err(|e| EgressError::DnsResolutionFailed {
                            hostname: host.clone(),
                            source: e,
                        })?
                        .map(|addr| EgressDestination::Ip {
                            ip: addr.ip(),
                            port: addr.port(),
                        })
                        .collect();
                if dests.is_empty() {
                    return Err(EgressError::DnsResolutionFailed {
                        hostname: host,
                        source: std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no addresses found",
                        ),
                    });
                }
                resolved.extend(dests);
            }
        }
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
    // The cgroup v2 line in /proc/<pid>/cgroup is "0::<relative-path>".
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = content.lines().find_map(|line| line.strip_prefix("0::"))?;
    cgroup_id_of_path(std::path::Path::new(&format!("/sys/fs/cgroup{rel}")))
}

/// Stub off Linux: there are no cgroups to resolve.
#[cfg(not(target_os = "linux"))]
pub fn cgroup_id_of_pid(_pid: u32) -> Option<u64> {
    None
}

/// Resolve the cgroup v2 id of a cgroup *directory* — the same inode
/// number `bpf_get_current_cgroup_id()` reports for processes inside it.
/// This is what lets egress be programmed *before* a process starts:
/// create the cgroup directory, program the maps against its inode, then
/// start the workload into it.
#[cfg(target_os = "linux")]
pub fn cgroup_id_of_path(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.ino())
}

/// Stub off Linux: there are no cgroups to resolve.
#[cfg(not(target_os = "linux"))]
pub fn cgroup_id_of_path(_path: &std::path::Path) -> Option<u64> {
    None
}

/// What to do about an instance's egress policy before its process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreStartEgress {
    /// No allowlist configured: nothing to program.
    NoPolicy,
    /// eBPF is not loaded: egress is documented-unenforced (D5), warn.
    Unenforced,
    /// The runtime does not place workloads into the computed cgroup
    /// path; defer to the post-start (pid-based) programming path.
    Deferred,
    /// Program the maps against this cgroup id, then start.
    Program { cgroup_id: u64 },
    /// Enforcement is required but impossible: fail the deploy closed.
    Refuse { reason: String },
}

/// Decide the pre-start egress step for an instance (pure logic, so the
/// fail-closed branches are testable without a kernel).
pub fn plan_pre_start_egress(
    has_allowlist: bool,
    ebpf_loaded: bool,
    connect6_attached: bool,
    runtime_honours_cgroup_path: bool,
    cgroup_id: Option<u64>,
) -> PreStartEgress {
    if !has_allowlist {
        return PreStartEgress::NoPolicy;
    }
    if !ebpf_loaded {
        return PreStartEgress::Unenforced;
    }
    if !connect6_attached {
        // A v4-only allowlist is trivially bypassable over IPv6 — refusing
        // the deploy is the only honest answer (NET7).
        return PreStartEgress::Refuse {
            reason: "egress allowlist requires the connect6 hook; \
                     an IPv4-only policy is bypassable over IPv6"
                .to_string(),
        };
    }
    if !runtime_honours_cgroup_path {
        return PreStartEgress::Deferred;
    }
    match cgroup_id {
        Some(cgroup_id) => PreStartEgress::Program { cgroup_id },
        None => PreStartEgress::Refuse {
            reason: "could not resolve the instance cgroup id before start".to_string(),
        },
    }
}

/// The result of comparing kernel egress state against live instances:
/// cgroup ids to scrub and cgroup ids whose entries need reinstalling.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EgressSweepPlan {
    /// Cgroups present in the kernel maps with no live instance: delete
    /// their allow entries and enforcement flag.
    pub stale: Vec<u64>,
    /// Live enforced cgroups missing from the kernel (e.g. lost across a
    /// Bun restart): rewrite their entries.
    pub repair: Vec<u64>,
}

impl EgressSweepPlan {
    /// True when the sweep has nothing to do (stays silent).
    pub fn is_empty(&self) -> bool {
        self.stale.is_empty() && self.repair.is_empty()
    }
}

/// Plan a kernel-truth reconciliation sweep: `expected` is the set of
/// cgroup ids that *should* be enforced (live instances with an
/// allowlist), `kernel_enforced` the ids with the enforcement flag set,
/// `kernel_entries` the ids owning at least one allow entry. Stale ids
/// (present anywhere in the kernel, expected nowhere) are scrubbed;
/// repair ids (expected but flag lost) are logged and reinstalled.
pub fn plan_egress_sweep(
    expected: &HashSet<u64>,
    kernel_enforced: &HashSet<u64>,
    kernel_entries: &HashSet<u64>,
) -> EgressSweepPlan {
    let kernel_all: HashSet<u64> = kernel_enforced.union(kernel_entries).copied().collect();
    let mut stale: Vec<u64> = kernel_all.difference(expected).copied().collect();
    let mut repair: Vec<u64> = expected.difference(kernel_enforced).copied().collect();
    stale.sort_unstable();
    repair.sort_unstable();
    EgressSweepPlan { stale, repair }
}

/// Value written to `egress_enabled_map` to turn on enforcement.
pub const EGRESS_ENFORCE: u32 = 1;

/// eBPF egress map writers. With the `ebpf` feature these write the
/// exact-match maps (`egress_map`, `egress6_map`), the CIDR LPM tries
/// (`egress_cidr4_map`, `egress_cidr6_map`) and `egress_enabled_map`
/// (per-cgroup enforcement flag); without it they return `Unsupported`.
#[cfg(feature = "ebpf")]
mod maps {
    use std::collections::HashSet;
    use std::net::IpAddr;

    use aya::maps::HashMap;
    use aya::maps::lpm_trie::{Key, LpmTrie};

    use super::{
        CIDR_CGROUP_PREFIX_BITS, CidrAllow, EGRESS_ENFORCE, Egress6Key, EgressCidrValue,
        EgressDestination, EgressKey, EgressMapError, EgressValue, cidr_value, cidr4_lpm_data,
        cidr6_lpm_data, exact_v4_key, exact_v6_key,
    };

    fn map_handle<'a>(
        bpf: &'a mut aya::Ebpf,
        name: &'static str,
    ) -> Result<&'a mut aya::maps::Map, EgressMapError> {
        bpf.map_mut(name)
            .ok_or(EgressMapError::MapNotFound { map_name: name })
    }

    /// Enable egress enforcement for a cgroup: with this set, any
    /// destination not present in the allow maps for the cgroup is denied.
    pub fn set_egress_enforced(bpf: &mut aya::Ebpf, cgroup_id: u64) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, u64, u32> =
            HashMap::try_from(map_handle(bpf, "egress_enabled_map")?)?;
        map.insert(cgroup_id, EGRESS_ENFORCE, 0)?;
        Ok(())
    }

    /// Turn egress enforcement off for a cgroup (used on instance stop).
    pub fn clear_egress_enforced(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, u64, u32> =
            HashMap::try_from(map_handle(bpf, "egress_enabled_map")?)?;
        let _ = map.remove(&cgroup_id);
        Ok(())
    }

    /// Allow a single (cgroup, dst_ip, dst_port) IPv4 destination.
    pub fn write_egress_entry(
        bpf: &mut aya::Ebpf,
        key: EgressKey,
        value: EgressValue,
    ) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, EgressKey, EgressValue> =
            HashMap::try_from(map_handle(bpf, "egress_map")?)?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Remove a previously allowed IPv4 destination.
    pub fn delete_egress_entry(bpf: &mut aya::Ebpf, key: EgressKey) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, EgressKey, EgressValue> =
            HashMap::try_from(map_handle(bpf, "egress_map")?)?;
        let _ = map.remove(&key);
        Ok(())
    }

    /// Program a full destination set for a cgroup: exact IPv4/IPv6
    /// entries plus the merged CIDR trie entries. Attempts every write
    /// and returns the first error afterwards, so one failed write
    /// leaves that destination denied without abandoning the rest.
    pub fn write_egress_destinations(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
        dests: &[EgressDestination],
        cidrs: &[CidrAllow],
    ) -> Result<(), EgressMapError> {
        let mut first_error: Option<EgressMapError> = None;
        let mut record = |result: Result<(), EgressMapError>| {
            if let Err(e) = result
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        };

        for dest in dests {
            if let EgressDestination::Ip { ip, port } = dest {
                match ip {
                    IpAddr::V4(v4) => record(write_egress_entry(
                        bpf,
                        exact_v4_key(cgroup_id, *v4, *port),
                        EgressValue {
                            action: super::EGRESS_ALLOW,
                        },
                    )),
                    IpAddr::V6(v6) => record(write_egress6_entry(
                        bpf,
                        exact_v6_key(cgroup_id, *v6, *port),
                        EgressValue {
                            action: super::EGRESS_ALLOW,
                        },
                    )),
                }
            }
        }

        for cidr in cidrs {
            let value = cidr_value(&cidr.ports);
            match cidr.network {
                IpAddr::V4(net) => {
                    let result = (|| {
                        let mut trie: LpmTrie<_, [u8; 12], EgressCidrValue> =
                            LpmTrie::try_from(map_handle(bpf, "egress_cidr4_map")?)?;
                        let key = Key::new(
                            CIDR_CGROUP_PREFIX_BITS + u32::from(cidr.prefix_len),
                            cidr4_lpm_data(cgroup_id, net),
                        );
                        trie.insert(&key, value, 0)?;
                        Ok(())
                    })();
                    record(result);
                }
                IpAddr::V6(net) => {
                    let result = (|| {
                        let mut trie: LpmTrie<_, [u8; 24], EgressCidrValue> =
                            LpmTrie::try_from(map_handle(bpf, "egress_cidr6_map")?)?;
                        let key = Key::new(
                            CIDR_CGROUP_PREFIX_BITS + u32::from(cidr.prefix_len),
                            cidr6_lpm_data(cgroup_id, net),
                        );
                        trie.insert(&key, value, 0)?;
                        Ok(())
                    })();
                    record(result);
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Write a single exact IPv6 allow entry.
    pub fn write_egress6_entry(
        bpf: &mut aya::Ebpf,
        key: Egress6Key,
        value: EgressValue,
    ) -> Result<(), EgressMapError> {
        let mut map: HashMap<_, Egress6Key, EgressValue> =
            HashMap::try_from(map_handle(bpf, "egress6_map")?)?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Delete every allow entry (exact and CIDR, both families) whose
    /// key belongs to `cgroup_id`. Leaves the enforcement flag alone.
    pub fn delete_cgroup_egress_entries(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), EgressMapError> {
        {
            let mut map: HashMap<_, EgressKey, EgressValue> =
                HashMap::try_from(map_handle(bpf, "egress_map")?)?;
            let keys: Vec<EgressKey> = map
                .keys()
                .filter_map(|k| k.ok())
                .filter(|k| k.src_cgroup_id == cgroup_id)
                .collect();
            for key in keys {
                let _ = map.remove(&key);
            }
        }
        {
            let mut map: HashMap<_, Egress6Key, EgressValue> =
                HashMap::try_from(map_handle(bpf, "egress6_map")?)?;
            let keys: Vec<Egress6Key> = map
                .keys()
                .filter_map(|k| k.ok())
                .filter(|k| k.src_cgroup_id == cgroup_id)
                .collect();
            for key in keys {
                let _ = map.remove(&key);
            }
        }
        let cgroup_be = cgroup_id.to_be_bytes();
        {
            let mut trie: LpmTrie<_, [u8; 12], EgressCidrValue> =
                LpmTrie::try_from(map_handle(bpf, "egress_cidr4_map")?)?;
            let keys: Vec<Key<[u8; 12]>> = trie
                .keys()
                .filter_map(|k| k.ok())
                .filter(|k| k.data()[..8] == cgroup_be)
                .collect();
            for key in keys {
                let _ = trie.remove(&key);
            }
        }
        {
            let mut trie: LpmTrie<_, [u8; 24], EgressCidrValue> =
                LpmTrie::try_from(map_handle(bpf, "egress_cidr6_map")?)?;
            let keys: Vec<Key<[u8; 24]>> = trie
                .keys()
                .filter_map(|k| k.ok())
                .filter(|k| k.data()[..8] == cgroup_be)
                .collect();
            for key in keys {
                let _ = trie.remove(&key);
            }
        }
        Ok(())
    }

    /// Scrub a cgroup completely: every allow entry plus the flag.
    pub fn delete_cgroup_egress_state(
        bpf: &mut aya::Ebpf,
        cgroup_id: u64,
    ) -> Result<(), EgressMapError> {
        delete_cgroup_egress_entries(bpf, cgroup_id)?;
        clear_egress_enforced(bpf, cgroup_id)
    }

    /// List every cgroup id with the enforcement flag set.
    pub fn list_enforced_cgroups(bpf: &mut aya::Ebpf) -> Result<HashSet<u64>, EgressMapError> {
        let map: HashMap<_, u64, u32> = HashMap::try_from(map_handle(bpf, "egress_enabled_map")?)?;
        Ok(map.keys().filter_map(|k| k.ok()).collect())
    }

    /// List every cgroup id that owns at least one allow entry in any of
    /// the four allow maps.
    pub fn list_egress_entry_cgroups(bpf: &mut aya::Ebpf) -> Result<HashSet<u64>, EgressMapError> {
        let mut cgroups = HashSet::new();
        {
            let map: HashMap<_, EgressKey, EgressValue> =
                HashMap::try_from(map_handle(bpf, "egress_map")?)?;
            cgroups.extend(map.keys().filter_map(|k| k.ok()).map(|k| k.src_cgroup_id));
        }
        {
            let map: HashMap<_, Egress6Key, EgressValue> =
                HashMap::try_from(map_handle(bpf, "egress6_map")?)?;
            cgroups.extend(map.keys().filter_map(|k| k.ok()).map(|k| k.src_cgroup_id));
        }
        {
            let trie: LpmTrie<_, [u8; 12], EgressCidrValue> =
                LpmTrie::try_from(map_handle(bpf, "egress_cidr4_map")?)?;
            cgroups.extend(trie.keys().filter_map(|k| k.ok()).map(|k| {
                let mut be = [0u8; 8];
                be.copy_from_slice(&k.data()[..8]);
                u64::from_be_bytes(be)
            }));
        }
        {
            let trie: LpmTrie<_, [u8; 24], EgressCidrValue> =
                LpmTrie::try_from(map_handle(bpf, "egress_cidr6_map")?)?;
            cgroups.extend(trie.keys().filter_map(|k| k.ok()).map(|k| {
                let mut be = [0u8; 8];
                be.copy_from_slice(&k.data()[..8]);
                u64::from_be_bytes(be)
            }));
        }
        Ok(cgroups)
    }

    /// Whether an IPv4 destination is currently allowed for a cgroup
    /// (reads `egress_map`). For tests and diagnostics.
    pub fn egress_allowed(bpf: &mut aya::Ebpf, key: EgressKey) -> Result<bool, EgressMapError> {
        let map: HashMap<_, EgressKey, EgressValue> =
            HashMap::try_from(map_handle(bpf, "egress_map")?)?;
        Ok(map.get(&key, 0).is_ok())
    }

    /// Whether egress enforcement is enabled for a cgroup (reads
    /// `egress_enabled_map`). For tests and diagnostics.
    pub fn egress_enforced(bpf: &mut aya::Ebpf, cgroup_id: u64) -> Result<bool, EgressMapError> {
        let map: HashMap<_, u64, u32> = HashMap::try_from(map_handle(bpf, "egress_enabled_map")?)?;
        Ok(map.get(&cgroup_id, 0).is_ok())
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

    #[error("CIDR {network} needs {count} ports; the kernel entry holds at most {limit}")]
    TooManyCidrPorts {
        network: String,
        count: usize,
        limit: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn ip_dest(ip: IpAddr, port: u16) -> EgressDestination {
        EgressDestination::Ip { ip, port }
    }

    fn cidr_dest(network: IpAddr, prefix_len: u8, port: u16) -> EgressDestination {
        EgressDestination::Cidr {
            network,
            prefix_len,
            port,
        }
    }

    // -- parsing: exact addresses ------------------------------------------

    #[test]
    fn parse_ip_port() {
        let result = parse_egress_entry("1.2.3.4:443").unwrap();
        assert_eq!(result, vec![ip_dest(v4(1, 2, 3, 4), 443)]);
    }

    #[test]
    fn parse_ip_port_with_whitespace() {
        let result = parse_egress_entry("  10.0.1.5:8080  ").unwrap();
        assert_eq!(result, vec![ip_dest(v4(10, 0, 1, 5), 8080)]);
    }

    #[test]
    fn parse_invalid_port() {
        assert!(parse_egress_entry("1.2.3.4:abc").is_err());
    }

    #[test]
    fn parse_missing_port() {
        assert!(parse_egress_entry("1.2.3.4").is_err());
    }

    #[test]
    fn parse_bracketed_ipv6() {
        let result = parse_egress_entry("[2001:db8::1]:443").unwrap();
        assert_eq!(
            result,
            vec![ip_dest(IpAddr::V6("2001:db8::1".parse().unwrap()), 443)]
        );
    }

    #[test]
    fn parse_unbracketed_ipv6_rejected_with_hint() {
        let err = parse_egress_entry("2001:db8::1:443").unwrap_err();
        assert!(err.to_string().contains("bracketed"), "got: {err}");
    }

    #[test]
    fn parse_unterminated_bracket_rejected() {
        assert!(parse_egress_entry("[2001:db8::1:443").is_err());
    }

    // -- parsing: CIDR ------------------------------------------------------

    #[test]
    fn parse_v4_cidr() {
        let result = parse_egress_entry("10.0.0.0/8:443").unwrap();
        assert_eq!(result, vec![cidr_dest(v4(10, 0, 0, 0), 8, 443)]);
    }

    #[test]
    fn parse_v4_cidr_zero_prefix() {
        let result = parse_egress_entry("0.0.0.0/0:53").unwrap();
        assert_eq!(result, vec![cidr_dest(v4(0, 0, 0, 0), 0, 53)]);
    }

    #[test]
    fn parse_v4_cidr_slash32() {
        let result = parse_egress_entry("192.168.1.7/32:80").unwrap();
        assert_eq!(result, vec![cidr_dest(v4(192, 168, 1, 7), 32, 80)]);
    }

    #[test]
    fn parse_v4_cidr_prefix_out_of_range() {
        let err = parse_egress_entry("10.0.0.0/33:443").unwrap_err();
        assert!(err.to_string().contains("out of range"), "got: {err}");
    }

    #[test]
    fn parse_v4_cidr_host_bits_rejected_with_suggestion() {
        let err = parse_egress_entry("10.1.2.3/8:443").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("host bits"), "got: {message}");
        assert!(message.contains("10.0.0.0/8"), "got: {message}");
    }

    #[test]
    fn parse_v6_cidr() {
        let result = parse_egress_entry("[2001:db8::/32]:443").unwrap();
        assert_eq!(
            result,
            vec![cidr_dest(
                IpAddr::V6("2001:db8::".parse().unwrap()),
                32,
                443
            )]
        );
    }

    #[test]
    fn parse_v6_cidr_prefix_out_of_range() {
        assert!(parse_egress_entry("[2001:db8::/129]:443").is_err());
    }

    #[test]
    fn parse_v6_cidr_host_bits_rejected_with_suggestion() {
        let err = parse_egress_entry("[2001:db8::1/32]:443").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("host bits"), "got: {message}");
        assert!(message.contains("2001:db8::/32"), "got: {message}");
    }

    // -- resolution ----------------------------------------------------------

    #[test]
    fn resolve_empty_list() {
        assert!(resolve_egress_entries(&[]).unwrap().is_empty());
    }

    #[test]
    fn resolve_multiple_entries_mixed_families() {
        let entries = vec![
            "1.2.3.4:443".to_string(),
            "[2001:db8::5]:80".to_string(),
            "10.0.0.0/8:6379".to_string(),
        ];
        let result = resolve_egress_entries(&entries).unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains(&ip_dest(v4(1, 2, 3, 4), 443)));
        assert!(result.contains(&ip_dest(IpAddr::V6("2001:db8::5".parse().unwrap()), 80)));
        assert!(result.contains(&cidr_dest(v4(10, 0, 0, 0), 8, 6379)));
    }

    // -- diff ----------------------------------------------------------------

    #[test]
    fn egress_diff_reports_added_and_removed() {
        let a = ip_dest(v4(1, 1, 1, 1), 443);
        let b = ip_dest(IpAddr::V6("2001:db8::2".parse().unwrap()), 443);
        let c = cidr_dest(v4(10, 0, 0, 0), 8, 443);

        // b stays, a is removed, c is added.
        let (add, remove) = egress_diff(&[a, b], &[b, c]);
        assert_eq!(add, vec![c]);
        assert_eq!(remove, vec![a]);
    }

    #[test]
    fn egress_diff_empty_when_unchanged() {
        let a = ip_dest(v4(1, 1, 1, 1), 443);
        let b = ip_dest(v4(2, 2, 2, 2), 80);
        let (add, remove) = egress_diff(&[a, b], &[b, a]);
        assert!(add.is_empty());
        assert!(remove.is_empty());
    }

    // -- CIDR port merging -----------------------------------------------------

    #[test]
    fn merge_folds_enclosing_ports_into_nested_prefix() {
        let dests = vec![
            cidr_dest(v4(10, 0, 0, 0), 8, 443),
            cidr_dest(v4(10, 1, 0, 0), 16, 80),
        ];
        let merged = merge_cidr_ports(&dests).unwrap();
        let nested = merged
            .iter()
            .find(|c| c.prefix_len == 16)
            .expect("nested entry present");
        // Without folding, an LPM lookup for 10.1.x.y:443 would match the
        // /16 entry and deny — the /8's port must be folded in.
        assert_eq!(nested.ports, vec![80, 443]);
        let outer = merged.iter().find(|c| c.prefix_len == 8).unwrap();
        assert_eq!(outer.ports, vec![443]);
    }

    #[test]
    fn merge_combines_ports_of_identical_cidrs() {
        let dests = vec![
            cidr_dest(v4(10, 0, 0, 0), 8, 443),
            cidr_dest(v4(10, 0, 0, 0), 8, 80),
            cidr_dest(v4(10, 0, 0, 0), 8, 443),
        ];
        let merged = merge_cidr_ports(&dests).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].ports, vec![80, 443]);
    }

    #[test]
    fn merge_ignores_exact_destinations_and_other_family() {
        let dests = vec![
            ip_dest(v4(1, 1, 1, 1), 443),
            cidr_dest(v4(10, 0, 0, 0), 8, 443),
            cidr_dest(IpAddr::V6("2001:db8::".parse().unwrap()), 32, 80),
        ];
        let merged = merge_cidr_ports(&dests).unwrap();
        assert_eq!(merged.len(), 2);
        // The v6 CIDR must not absorb the v4 /8's ports.
        let v6_entry = merged
            .iter()
            .find(|c| matches!(c.network, IpAddr::V6(_)))
            .unwrap();
        assert_eq!(v6_entry.ports, vec![80]);
    }

    #[test]
    fn merge_rejects_port_overflow() {
        let dests: Vec<EgressDestination> =
            (1..=9).map(|p| cidr_dest(v4(10, 0, 0, 0), 8, p)).collect();
        let err = merge_cidr_ports(&dests).unwrap_err();
        assert!(matches!(err, EgressError::TooManyCidrPorts { .. }));
    }

    // -- map entry layout -----------------------------------------------------

    #[test]
    fn egress_key_size() {
        assert_eq!(std::mem::size_of::<EgressKey>(), 16);
    }

    #[test]
    fn egress6_key_size() {
        assert_eq!(std::mem::size_of::<Egress6Key>(), 32);
    }

    #[test]
    fn egress_value_size() {
        assert_eq!(std::mem::size_of::<EgressValue>(), 4);
    }

    #[test]
    fn cidr_value_size() {
        assert_eq!(std::mem::size_of::<EgressCidrValue>(), 20);
    }

    #[test]
    fn exact_v4_key_is_network_byte_order() {
        let key = exact_v4_key(42, Ipv4Addr::new(1, 2, 3, 4), 443);
        assert_eq!(key.src_cgroup_id, 42);
        assert_eq!(key.dst_ip, u32::from_be_bytes([1, 2, 3, 4]).to_be());
        assert_eq!(key.dst_port, 443u16.to_be());
    }

    #[test]
    fn exact_v6_key_holds_octets_and_be_port() {
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let key = exact_v6_key(7, ip, 8080);
        assert_eq!(key.dst_ip, ip.octets());
        assert_eq!(key.dst_port, 8080u16.to_be());
    }

    #[test]
    fn cidr4_lpm_data_layout_big_endian_cgroup_then_network() {
        let data = cidr4_lpm_data(0x0102030405060708, Ipv4Addr::new(10, 0, 0, 0));
        assert_eq!(&data[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&data[8..], &[10, 0, 0, 0]);
    }

    #[test]
    fn cidr6_lpm_data_layout_big_endian_cgroup_then_network() {
        let net: Ipv6Addr = "2001:db8::".parse().unwrap();
        let data = cidr6_lpm_data(0xAABB, net);
        assert_eq!(&data[..8], &0xAABBu64.to_be_bytes());
        assert_eq!(&data[8..], &net.octets());
    }

    #[test]
    fn cidr_value_ports_in_network_byte_order() {
        let value = cidr_value(&[443, 80]);
        assert_eq!(value.count, 2);
        assert_eq!(value.ports[0], 443u16.to_be());
        assert_eq!(value.ports[1], 80u16.to_be());
        assert_eq!(value.ports[2], 0);
    }

    // -- pre-start planning -----------------------------------------------------

    #[test]
    fn pre_start_no_allowlist_means_no_policy() {
        assert_eq!(
            plan_pre_start_egress(false, true, true, true, Some(1)),
            PreStartEgress::NoPolicy
        );
    }

    #[test]
    fn pre_start_without_ebpf_is_unenforced() {
        assert_eq!(
            plan_pre_start_egress(true, false, false, true, None),
            PreStartEgress::Unenforced
        );
    }

    #[test]
    fn pre_start_refuses_deploy_without_connect6() {
        // A v4-only allowlist is bypassable over IPv6: fail the deploy closed.
        let plan = plan_pre_start_egress(true, true, false, true, Some(1));
        assert!(
            matches!(plan, PreStartEgress::Refuse { .. }),
            "got {plan:?}"
        );
    }

    #[test]
    fn pre_start_defers_when_runtime_ignores_cgroup_path() {
        assert_eq!(
            plan_pre_start_egress(true, true, true, false, None),
            PreStartEgress::Deferred
        );
    }

    #[test]
    fn pre_start_programs_with_resolved_cgroup() {
        assert_eq!(
            plan_pre_start_egress(true, true, true, true, Some(99)),
            PreStartEgress::Program { cgroup_id: 99 }
        );
    }

    #[test]
    fn pre_start_refuses_when_cgroup_unresolvable() {
        let plan = plan_pre_start_egress(true, true, true, true, None);
        assert!(
            matches!(plan, PreStartEgress::Refuse { .. }),
            "got {plan:?}"
        );
    }

    // -- sweep planning ----------------------------------------------------------

    #[test]
    fn sweep_plans_stale_and_repair_sorted() {
        let expected: HashSet<u64> = [10, 20, 30].into_iter().collect();
        let enforced: HashSet<u64> = [20, 99].into_iter().collect();
        let entries: HashSet<u64> = [20, 5].into_iter().collect();
        let plan = plan_egress_sweep(&expected, &enforced, &entries);
        // 5 and 99 exist in the kernel with no live instance: scrub.
        assert_eq!(plan.stale, vec![5, 99]);
        // 10 and 30 lost their enforcement flag: reinstall.
        assert_eq!(plan.repair, vec![10, 30]);
        assert!(!plan.is_empty());
    }

    #[test]
    fn sweep_flags_orphaned_entries_even_when_flag_is_clear() {
        // Allow entries without an enforcement flag are still stale kernel
        // state a recycled cgroup id could inherit.
        let expected: HashSet<u64> = HashSet::new();
        let enforced: HashSet<u64> = HashSet::new();
        let entries: HashSet<u64> = [7].into_iter().collect();
        let plan = plan_egress_sweep(&expected, &enforced, &entries);
        assert_eq!(plan.stale, vec![7]);
    }

    #[test]
    fn sweep_is_silent_when_kernel_matches() {
        let expected: HashSet<u64> = [1, 2].into_iter().collect();
        let plan = plan_egress_sweep(&expected, &expected.clone(), &expected.clone());
        assert!(plan.is_empty());
    }

    // -- cgroup id helpers ---------------------------------------------------------

    #[test]
    fn cgroup_id_of_missing_path_is_none() {
        assert_eq!(
            cgroup_id_of_path(std::path::Path::new("/nonexistent/cgroup/path")),
            None
        );
    }
}

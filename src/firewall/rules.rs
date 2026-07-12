/// Perimeter firewall rules via nftables.
///
/// Protects Reliaburger's own ports from external access. We only
/// block our ports (container host ports, cluster ports, management
/// port) — everything else (SSH, operator services) is untouched.
///
/// The chain policy is `accept`. We add explicit `drop` rules for
/// our port ranges from non-cluster, non-admin sources. This way
/// we never lock operators out of their own machines.
///
/// Rules are rendered for both the `ip` and `ip6` families (NET8): an
/// IPv4-only perimeter left every drop rule void over IPv6. All
/// administrator-supplied CIDRs are parsed and re-serialised before
/// they reach the ruleset — raw config strings are never interpolated
/// into `nft -f` input.
use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Configuration for the perimeter firewall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerimeterConfig {
    /// Whether the firewall is enabled. Defaults to `true` on Linux
    /// with root, `false` in rootless mode (nftables needs CAP_NET_ADMIN).
    pub enabled: bool,
    /// Container host port range (dynamically allocated, default: 30000-31000).
    /// External access to these ports is blocked — traffic reaches
    /// containers via Wrapper on the ingress ports instead.
    pub host_port_range: (u16, u16),
    /// Cluster communication ports (gossip, Raft, reporting).
    /// Only accessible from cluster node IPs.
    pub cluster_ports: Vec<u16>,
    /// Admin CIDRs allowed to reach the management port. IPv4 and IPv6
    /// accepted; a bare address means a single host.
    pub admin_cidrs: Vec<String>,
    /// Management port (Bun API, default: 9117).
    /// Accessible from cluster nodes and admin CIDRs.
    pub management_port: u16,
}

impl Default for PerimeterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host_port_range: (30000, 31000),
            cluster_ports: vec![9443, 9444, 9445],
            admin_cidrs: Vec::new(),
            management_port: 9117,
        }
    }
}

impl PerimeterConfig {
    /// Config for rootless mode: firewall disabled since nftables
    /// requires CAP_NET_ADMIN which non-root users don't have.
    pub fn for_rootless() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Errors from firewall operations.
#[derive(Debug, thiserror::Error)]
pub enum FirewallError {
    #[error("failed to apply firewall rules: {reason}")]
    ApplyFailed { reason: String },

    #[error("invalid CIDR {value:?}: {reason}")]
    InvalidCidr { value: String, reason: String },
}

/// The set of cluster node IPs (maintained from gossip membership).
pub type ClusterNodes = BTreeSet<IpAddr>;

/// How long a single `nft` invocation may take before we give up.
#[cfg(target_os = "linux")]
const NFT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse an administrator-supplied CIDR (or bare address) into a
/// validated `(address, prefix length)` pair. Rendering only ever
/// re-serialises this — a config value like `10.0.0.0/8; drop` is a
/// parse error, not an injected rule.
pub fn parse_cidr(value: &str) -> Result<(IpAddr, u8), FirewallError> {
    let trimmed = value.trim();

    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        let host_len = if ip.is_ipv4() { 32 } else { 128 };
        return Ok((ip, host_len));
    }

    let (address_str, prefix_str) =
        trimmed
            .split_once('/')
            .ok_or_else(|| FirewallError::InvalidCidr {
                value: value.to_string(),
                reason: "expected address or address/prefix".to_string(),
            })?;

    let address: IpAddr = address_str
        .parse()
        .map_err(|_| FirewallError::InvalidCidr {
            value: value.to_string(),
            reason: format!("invalid address: {address_str}"),
        })?;
    let prefix_len: u8 = prefix_str.parse().map_err(|_| FirewallError::InvalidCidr {
        value: value.to_string(),
        reason: format!("invalid prefix length: {prefix_str}"),
    })?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    if prefix_len > max {
        return Err(FirewallError::InvalidCidr {
            value: value.to_string(),
            reason: format!("prefix length {prefix_len} out of range (0-{max})"),
        });
    }
    Ok((address, prefix_len))
}

/// Serialise a validated CIDR back to nftables syntax.
fn render_cidr(address: IpAddr, prefix_len: u8) -> String {
    format!("{address}/{prefix_len}")
}

/// Render the input chain for one family (`ip` or `ip6`).
fn render_family_table(
    family: &str,
    saddr_keyword: &str,
    config: &PerimeterConfig,
    node_ips: &[IpAddr],
    admin_cidrs: &[(IpAddr, u8)],
) -> String {
    let mut rules = String::new();

    // Make re-application idempotent. `nft -f` *appends* to an existing table,
    // so reconciling on every membership change would stack fresh rules behind
    // stale ones — and the earliest copy (generated before gossip discovered
    // the peers, when only this node's own IP was known) would drop peer
    // traffic on the cluster ports before it ever reached the current
    // allow-members rule. Ensure the table exists, then delete it, so each
    // apply recreates the whole ruleset from a clean slate in one atomic
    // transaction.
    //
    // This is a table of its own (`reliaburger_fw`), separate from the
    // `reliaburger` table that `grill::netns` uses for container port-mapping
    // DNAT and masquerade. Flushing our own table on every reconcile must not
    // wipe the NAT chains that keep running containers reachable.
    rules.push_str(&format!("table {family} reliaburger_fw {{}}\n"));
    rules.push_str(&format!("delete table {family} reliaburger_fw\n"));

    rules.push_str(&format!("table {family} reliaburger_fw {{\n"));
    rules.push_str("  chain input {\n");
    rules.push_str("    type filter hook input priority 0; policy accept;\n");
    rules.push('\n');

    // Always allow loopback. Without this, the `tcp dport … drop` rules below
    // also drop traffic to 127.0.0.1 / ::1 — so `relish` on the node (which
    // talks to the local agent on loopback) and any local health check would
    // hang.
    rules.push_str("    # Always allow loopback (local relish, health checks)\n");
    rules.push_str("    iif \"lo\" accept\n");
    rules.push('\n');

    // Allow cluster node IPs to reach everything (inter-node traffic)
    if !node_ips.is_empty() {
        let ips: Vec<String> = node_ips.iter().map(|ip| ip.to_string()).collect();
        let ip_set = ips.join(", ");
        rules.push_str("    # Allow all traffic from cluster nodes\n");
        rules.push_str(&format!(
            "    {saddr_keyword} saddr {{ {ip_set} }} accept\n"
        ));
        rules.push('\n');
    }

    // Allow admin CIDRs to reach management port
    if !admin_cidrs.is_empty() {
        let cidrs: Vec<String> = admin_cidrs
            .iter()
            .map(|(address, len)| render_cidr(*address, *len))
            .collect();
        let cidr_set = cidrs.join(", ");
        rules.push_str("    # Allow management from admin CIDRs\n");
        rules.push_str(&format!(
            "    {saddr_keyword} saddr {{ {cidr_set} }} tcp dport {} accept\n",
            config.management_port
        ));
        rules.push('\n');
    }

    // Block external access to container host port range
    let (port_start, port_end) = config.host_port_range;
    rules.push_str("    # Block external access to container host ports\n");
    rules.push_str(&format!("    tcp dport {port_start}-{port_end} drop\n"));
    rules.push('\n');

    // Block external access to cluster ports on both protocols. Gossip is
    // UDP (M18: a tcp-only drop left the gossip port externally reachable);
    // Raft/reporting are TCP. Cluster members bypass these via the
    // `saddr … accept` rule above, so dropping both is safe.
    for port in &config.cluster_ports {
        rules.push_str(&format!("    tcp dport {port} drop\n"));
        rules.push_str(&format!("    udp dport {port} drop\n"));
    }
    rules.push('\n');

    // Block external access to management port
    rules.push_str(&format!("    tcp dport {} drop\n", config.management_port));
    rules.push('\n');

    // Everything else passes through (policy accept)
    rules.push_str("    # Everything else: accept (SSH, operator services, etc.)\n");
    rules.push_str("  }\n");
    rules.push_str("}\n");

    rules
}

/// Generate the nftables ruleset for the perimeter firewall — one
/// `reliaburger_fw` table per family (`ip` and `ip6`), with family-
/// appropriate source addresses and identical port policy.
///
/// Policy is `accept` — we only drop traffic to Reliaburger's own
/// ports from non-authorised sources. SSH and everything else the
/// operator runs is untouched.
///
/// Errors when an admin CIDR does not parse; nothing unvalidated is
/// ever rendered.
pub fn generate_ruleset(
    config: &PerimeterConfig,
    cluster_nodes: &ClusterNodes,
) -> Result<String, FirewallError> {
    let mut admin_v4: Vec<(IpAddr, u8)> = Vec::new();
    let mut admin_v6: Vec<(IpAddr, u8)> = Vec::new();
    for value in &config.admin_cidrs {
        let (address, prefix_len) = parse_cidr(value)?;
        match address {
            IpAddr::V4(_) => admin_v4.push((address, prefix_len)),
            IpAddr::V6(_) => admin_v6.push((address, prefix_len)),
        }
    }

    let nodes_v4: Vec<IpAddr> = cluster_nodes
        .iter()
        .copied()
        .filter(IpAddr::is_ipv4)
        .collect();
    let nodes_v6: Vec<IpAddr> = cluster_nodes
        .iter()
        .copied()
        .filter(IpAddr::is_ipv6)
        .collect();

    let mut rules = render_family_table("ip", "ip", config, &nodes_v4, &admin_v4);
    rules.push_str(&render_family_table(
        "ip6", "ip6", config, &nodes_v6, &admin_v6,
    ));
    Ok(rules)
}

/// Apply the firewall ruleset to the kernel via nftables. The `nft`
/// invocation is bounded by a timeout — a wedged nft must not hang the
/// agent event loop forever.
#[cfg(target_os = "linux")]
pub async fn apply_ruleset(ruleset: &str) -> Result<(), FirewallError> {
    use tokio::io::AsyncWriteExt;

    let apply = async {
        let mut child = tokio::process::Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| FirewallError::ApplyFailed {
                reason: format!("failed to spawn nft: {e}"),
            })?;

        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(ruleset.as_bytes())
                .await
                .map_err(|e| FirewallError::ApplyFailed {
                    reason: format!("failed to write ruleset to nft stdin: {e}"),
                })?;
        }
        // Drop stdin to signal EOF
        child.stdin.take();

        let result = child
            .wait_with_output()
            .await
            .map_err(|e| FirewallError::ApplyFailed {
                reason: format!("nft failed: {e}"),
            })?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(FirewallError::ApplyFailed {
                reason: format!("nft returned error: {stderr}"),
            });
        }

        Ok(())
    };

    tokio::time::timeout(NFT_TIMEOUT, apply)
        .await
        .map_err(|_| FirewallError::ApplyFailed {
            reason: format!("nft -f - timed out after {}s", NFT_TIMEOUT.as_secs()),
        })?
}

/// No-op on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub async fn apply_ruleset(_ruleset: &str) -> Result<(), FirewallError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> PerimeterConfig {
        PerimeterConfig::default()
    }

    fn cluster_with_nodes(ips: &[&str]) -> ClusterNodes {
        ips.iter().map(|s| s.parse::<IpAddr>().unwrap()).collect()
    }

    fn generate(config: &PerimeterConfig, nodes: &ClusterNodes) -> String {
        generate_ruleset(config, nodes).unwrap()
    }

    #[test]
    fn loopback_accepted_before_port_drops() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        // Loopback must be accepted, and before the management-port drop, or
        // `relish` on the node (talking to 127.0.0.1:9117) would be dropped.
        assert!(rules.contains("iif \"lo\" accept"));
        let lo_pos = rules.find("iif \"lo\" accept").unwrap();
        let drop_pos = rules.find("tcp dport 9117 drop").unwrap();
        assert!(lo_pos < drop_pos);
    }

    #[test]
    fn ruleset_flushes_table_before_redefining() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1"]);
        let rules = generate(&config, &nodes);

        // The ruleset must delete the table before recreating it, so that
        // re-applying on a membership change (nft -f appends) doesn't stack
        // stale rules ahead of the fresh ones. The delete must come before
        // the real chain definition.
        assert!(rules.contains("delete table ip reliaburger_fw"));
        let delete_pos = rules.find("delete table ip reliaburger_fw").unwrap();
        let chain_pos = rules.find("chain input").unwrap();
        assert!(delete_pos < chain_pos);
    }

    #[test]
    fn ruleset_uses_isolated_table_name() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1"]);
        let rules = generate(&config, &nodes);

        // The perimeter firewall must live in its own table so that flushing it
        // never touches the `reliaburger` table `grill::netns` uses for
        // container DNAT/masquerade. Guard against a regression that would
        // delete (and so wipe) that shared table.
        assert!(rules.contains("table ip reliaburger_fw"));
        assert!(!rules.contains("delete table ip reliaburger\n"));
        assert!(!rules.contains("table ip reliaburger {")); // no bare container table
    }

    #[test]
    fn policy_is_accept_not_drop() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("policy accept"));
        assert!(!rules.contains("policy drop"));
    }

    #[test]
    fn host_port_range_blocked() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("tcp dport 30000-31000 drop"));
    }

    #[test]
    fn cluster_ports_blocked_from_external() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("tcp dport 9443 drop"));
        assert!(rules.contains("tcp dport 9444 drop"));
        assert!(rules.contains("tcp dport 9445 drop"));
    }

    /// M18: cluster ports must be dropped on UDP too — gossip is UDP, and a
    /// tcp-only rule left the gossip port externally reachable.
    #[test]
    fn cluster_ports_blocked_on_udp_too() {
        let config = default_config();
        let rules = generate(&config, &ClusterNodes::new());
        for port in &config.cluster_ports {
            assert!(
                rules.contains(&format!("udp dport {port} drop")),
                "cluster port {port} not dropped on UDP"
            );
        }
    }

    #[test]
    fn management_port_blocked_from_external() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("tcp dport 9117 drop"));
    }

    #[test]
    fn cluster_nodes_bypass_all_blocks() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1", "10.0.1.2"]);
        let rules = generate(&config, &nodes);

        // Cluster nodes get a blanket accept before any drop rules
        assert!(rules.contains("ip saddr { 10.0.1.1, 10.0.1.2 } accept"));

        // The accept rule must come before the drop rules
        let accept_pos = rules.find("10.0.1.1, 10.0.1.2 } accept").unwrap();
        let drop_pos = rules.find("30000-31000 drop").unwrap();
        assert!(accept_pos < drop_pos);
    }

    #[test]
    fn admin_cidrs_reach_management_port() {
        let mut config = default_config();
        config.admin_cidrs = vec!["192.168.1.0/24".to_string()];
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("192.168.1.0/24"));
        assert!(rules.contains("tcp dport 9117 accept"));

        // Admin accept must come before the management port drop
        let accept_pos = rules.find("192.168.1.0/24").unwrap();
        let drop_pos = rules.find("tcp dport 9117 drop").unwrap();
        assert!(accept_pos < drop_pos);
    }

    #[test]
    fn ssh_not_mentioned() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        // SSH (port 22) should not appear in any rule
        assert!(!rules.contains("dport 22"));
    }

    #[test]
    fn custom_host_port_range() {
        let mut config = default_config();
        config.host_port_range = (40000, 41000);
        let nodes = ClusterNodes::new();
        let rules = generate(&config, &nodes);

        assert!(rules.contains("tcp dport 40000-41000 drop"));
        assert!(!rules.contains("30000"));
    }

    #[test]
    fn default_config_values() {
        let config = default_config();
        assert_eq!(config.host_port_range, (30000, 31000));
        assert_eq!(config.cluster_ports, vec![9443, 9444, 9445]);
        assert_eq!(config.management_port, 9117);
    }

    #[test]
    fn ruleset_is_valid_nft_structure() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1"]);
        let rules = generate(&config, &nodes);

        let open = rules.matches('{').count();
        let close = rules.matches('}').count();
        assert_eq!(open, close, "unbalanced braces in ruleset");

        assert!(rules.starts_with("table ip reliaburger"));
        assert!(rules.trim().ends_with('}'));
    }

    // -- NET8: safe input, dual-stack -----------------------------------------

    /// A config value carrying nft syntax must be a parse error, never an
    /// injected rule.
    #[test]
    fn malformed_admin_cidr_is_rejected_not_interpolated() {
        let mut config = default_config();
        config.admin_cidrs = vec!["10.0.0.0/8; drop".to_string()];
        let err = generate_ruleset(&config, &ClusterNodes::new()).unwrap_err();
        assert!(
            matches!(err, FirewallError::InvalidCidr { .. }),
            "got {err}"
        );
    }

    #[test]
    fn admin_cidr_prefix_out_of_range_rejected() {
        let mut config = default_config();
        config.admin_cidrs = vec!["10.0.0.0/33".to_string()];
        assert!(generate_ruleset(&config, &ClusterNodes::new()).is_err());
    }

    #[test]
    fn admin_cidr_garbage_rejected() {
        let mut config = default_config();
        config.admin_cidrs = vec!["not-a-cidr".to_string()];
        assert!(generate_ruleset(&config, &ClusterNodes::new()).is_err());
    }

    #[test]
    fn bare_admin_address_becomes_host_prefix() {
        let mut config = default_config();
        config.admin_cidrs = vec!["192.168.1.9".to_string()];
        let rules = generate(&config, &ClusterNodes::new());
        assert!(rules.contains("192.168.1.9/32"));
    }

    #[test]
    fn ruleset_renders_both_families() {
        let config = default_config();
        let rules = generate(&config, &ClusterNodes::new());
        assert!(rules.contains("table ip reliaburger_fw {\n"));
        assert!(rules.contains("table ip6 reliaburger_fw {\n"));
        // The port policy applies over IPv6 too — an ip-only perimeter
        // leaves every drop rule void for v6 traffic.
        let ip6_start = rules.find("table ip6 reliaburger_fw {\n").unwrap();
        let ip6_rules = &rules[ip6_start..];
        assert!(ip6_rules.contains("tcp dport 30000-31000 drop"));
        assert!(ip6_rules.contains("tcp dport 9117 drop"));
        assert!(ip6_rules.contains("udp dport 9443 drop"));
    }

    #[test]
    fn v6_admin_cidr_lands_only_in_ip6_table() {
        let mut config = default_config();
        config.admin_cidrs = vec!["2001:db8::/32".to_string()];
        let rules = generate(&config, &ClusterNodes::new());

        let ip6_start = rules.find("table ip6 reliaburger_fw {\n").unwrap();
        let (v4_half, v6_half) = rules.split_at(ip6_start);
        assert!(!v4_half.contains("2001:db8::/32"));
        assert!(v6_half.contains("ip6 saddr { 2001:db8::/32 } tcp dport 9117 accept"));
    }

    #[test]
    fn v6_cluster_node_lands_only_in_ip6_table() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1", "fd00::1"]);
        let rules = generate(&config, &nodes);

        let ip6_start = rules.find("table ip6 reliaburger_fw {\n").unwrap();
        let (v4_half, v6_half) = rules.split_at(ip6_start);
        assert!(v4_half.contains("ip saddr { 10.0.1.1 } accept"));
        assert!(!v4_half.contains("fd00::1"));
        assert!(v6_half.contains("ip6 saddr { fd00::1 } accept"));
    }

    #[test]
    fn parse_cidr_accepts_v4_v6_and_bare_addresses() {
        assert_eq!(
            parse_cidr("10.0.0.0/8").unwrap(),
            ("10.0.0.0".parse().unwrap(), 8)
        );
        assert_eq!(
            parse_cidr(" 2001:db8::/32 ").unwrap(),
            ("2001:db8::".parse().unwrap(), 32)
        );
        assert_eq!(
            parse_cidr("fd00::1").unwrap(),
            ("fd00::1".parse().unwrap(), 128)
        );
    }
}

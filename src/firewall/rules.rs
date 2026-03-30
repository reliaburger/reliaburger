/// Perimeter firewall rules via nftables.
///
/// Protects Reliaburger's own ports from external access. We only
/// block our ports (container host ports, cluster ports, management
/// port) — everything else (SSH, operator services) is untouched.
///
/// The chain policy is `accept`. We add explicit `drop` rules for
/// our port ranges from non-cluster, non-admin sources. This way
/// we never lock operators out of their own machines.
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
    /// Admin CIDRs allowed to reach the management port.
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
}

/// The set of cluster node IPs (maintained from gossip membership).
pub type ClusterNodes = BTreeSet<IpAddr>;

/// Generate the nftables ruleset for the perimeter firewall.
///
/// Policy is `accept` — we only drop traffic to Reliaburger's own
/// ports from non-authorised sources. SSH and everything else the
/// operator runs is untouched.
pub fn generate_ruleset(config: &PerimeterConfig, cluster_nodes: &ClusterNodes) -> String {
    let mut rules = String::new();

    rules.push_str("table ip reliaburger {\n");
    rules.push_str("  chain input {\n");
    rules.push_str("    type filter hook input priority 0; policy accept;\n");
    rules.push('\n');

    // Allow cluster node IPs to reach everything (inter-node traffic)
    if !cluster_nodes.is_empty() {
        let ips: Vec<String> = cluster_nodes.iter().map(|ip| ip.to_string()).collect();
        let ip_set = ips.join(", ");
        rules.push_str("    # Allow all traffic from cluster nodes\n");
        rules.push_str(&format!("    ip saddr {{ {ip_set} }} accept\n"));
        rules.push('\n');
    }

    // Allow admin CIDRs to reach management port
    if !config.admin_cidrs.is_empty() {
        let cidrs = config.admin_cidrs.join(", ");
        rules.push_str("    # Allow management from admin CIDRs\n");
        rules.push_str(&format!(
            "    ip saddr {{ {cidrs} }} tcp dport {} accept\n",
            config.management_port
        ));
        rules.push('\n');
    }

    // Block external access to container host port range
    let (port_start, port_end) = config.host_port_range;
    rules.push_str("    # Block external access to container host ports\n");
    rules.push_str(&format!("    tcp dport {port_start}-{port_end} drop\n"));
    rules.push('\n');

    // Block external access to cluster ports
    for port in &config.cluster_ports {
        rules.push_str(&format!("    tcp dport {port} drop\n"));
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

/// Apply the firewall ruleset to the kernel via nftables.
#[cfg(target_os = "linux")]
pub async fn apply_ruleset(ruleset: &str) -> Result<(), FirewallError> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

    #[test]
    fn policy_is_accept_not_drop() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("policy accept"));
        assert!(!rules.contains("policy drop"));
    }

    #[test]
    fn host_port_range_blocked() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("tcp dport 30000-31000 drop"));
    }

    #[test]
    fn cluster_ports_blocked_from_external() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("tcp dport 9443 drop"));
        assert!(rules.contains("tcp dport 9444 drop"));
        assert!(rules.contains("tcp dport 9445 drop"));
    }

    #[test]
    fn management_port_blocked_from_external() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("tcp dport 9117 drop"));
    }

    #[test]
    fn cluster_nodes_bypass_all_blocks() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1", "10.0.1.2"]);
        let rules = generate_ruleset(&config, &nodes);

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
        let rules = generate_ruleset(&config, &nodes);

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
        let rules = generate_ruleset(&config, &nodes);

        // SSH (port 22) should not appear in any rule
        assert!(!rules.contains("dport 22"));
    }

    #[test]
    fn custom_host_port_range() {
        let mut config = default_config();
        config.host_port_range = (40000, 41000);
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

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
        let rules = generate_ruleset(&config, &nodes);

        let open = rules.matches('{').count();
        let close = rules.matches('}').count();
        assert_eq!(open, close, "unbalanced braces in ruleset");

        assert!(rules.starts_with("table ip reliaburger"));
        assert!(rules.trim().ends_with('}'));
    }
}

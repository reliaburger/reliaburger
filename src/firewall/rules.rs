/// Perimeter firewall rules via nftables.
///
/// Manages the `input` chain in the `reliaburger` nftables table.
/// Default policy is drop — only explicitly allowed traffic passes.
use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Configuration for the perimeter firewall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerimeterConfig {
    /// Ingress ports open to the world (default: 80, 443).
    pub ingress_ports: Vec<u16>,
    /// Cluster communication ports (gossip, Raft, reporting).
    pub cluster_ports: Vec<u16>,
    /// Admin CIDRs allowed to reach management ports.
    pub admin_cidrs: Vec<String>,
    /// Management port (Bun API, default: 9117).
    pub management_port: u16,
}

impl Default for PerimeterConfig {
    fn default() -> Self {
        Self {
            ingress_ports: vec![80, 443],
            cluster_ports: vec![9443, 9444, 9445],
            admin_cidrs: Vec::new(),
            management_port: 9117,
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
/// This is a pure function — it produces the ruleset as a string.
/// Applying it to the kernel is a separate step (Linux only).
pub fn generate_ruleset(config: &PerimeterConfig, cluster_nodes: &ClusterNodes) -> String {
    let mut rules = String::new();

    // Table and chain setup (idempotent)
    rules.push_str("table ip reliaburger {\n");

    // Input chain with default drop policy
    rules.push_str("  chain input {\n");
    rules.push_str("    type filter hook input priority 0; policy drop;\n");
    rules.push('\n');

    // Allow established/related connections
    rules.push_str("    ct state established,related accept\n");
    // Allow loopback
    rules.push_str("    iif lo accept\n");
    rules.push('\n');

    // Allow ICMP (ping)
    rules.push_str("    ip protocol icmp accept\n");
    rules.push('\n');

    // Allow ingress ports from anywhere
    for port in &config.ingress_ports {
        rules.push_str(&format!("    tcp dport {port} accept\n"));
    }
    rules.push('\n');

    // Allow cluster ports from cluster node IPs
    if !cluster_nodes.is_empty() {
        rules.push_str("    # Cluster inter-node traffic\n");
        let ips: Vec<String> = cluster_nodes.iter().map(|ip| ip.to_string()).collect();
        let ip_set = ips.join(", ");
        for port in &config.cluster_ports {
            rules.push_str(&format!(
                "    ip saddr {{ {ip_set} }} tcp dport {port} accept\n"
            ));
        }
        rules.push('\n');
    }

    // Allow management port from admin CIDRs and cluster nodes
    if !config.admin_cidrs.is_empty() {
        rules.push_str("    # Management access from admin CIDRs\n");
        let cidrs = config.admin_cidrs.join(", ");
        rules.push_str(&format!(
            "    ip saddr {{ {cidrs} }} tcp dport {} accept\n",
            config.management_port
        ));
        rules.push('\n');
    }

    // Allow management port from cluster nodes
    if !cluster_nodes.is_empty() {
        let ips: Vec<String> = cluster_nodes.iter().map(|ip| ip.to_string()).collect();
        let ip_set = ips.join(", ");
        rules.push_str(&format!(
            "    ip saddr {{ {ip_set} }} tcp dport {} accept\n",
            config.management_port
        ));
        rules.push('\n');
    }

    // Everything else is dropped by the chain policy
    rules.push_str("    # Default: drop\n");
    rules.push_str("  }\n");
    rules.push_str("}\n");

    rules
}

/// Apply the firewall ruleset to the kernel via nftables.
///
/// On non-Linux platforms, this is a no-op.
#[cfg(target_os = "linux")]
pub async fn apply_ruleset(ruleset: &str) -> Result<(), FirewallError> {
    let output = tokio::process::Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(ruleset.as_bytes())?;
            }
            Ok(child)
        })
        .map_err(|e| FirewallError::ApplyFailed {
            reason: format!("failed to spawn nft: {e}"),
        })?;

    let result = output
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
    fn empty_cluster_generates_basic_rules() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("policy drop"));
        assert!(rules.contains("ct state established,related accept"));
        assert!(rules.contains("iif lo accept"));
        assert!(rules.contains("tcp dport 80 accept"));
        assert!(rules.contains("tcp dport 443 accept"));
    }

    #[test]
    fn cluster_nodes_allowed_on_cluster_ports() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1", "10.0.1.2"]);
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("10.0.1.1"));
        assert!(rules.contains("10.0.1.2"));
        assert!(rules.contains("tcp dport 9443 accept"));
        assert!(rules.contains("tcp dport 9444 accept"));
        assert!(rules.contains("tcp dport 9445 accept"));
    }

    #[test]
    fn admin_cidrs_allowed_on_management_port() {
        let mut config = default_config();
        config.admin_cidrs = vec!["192.168.1.0/24".to_string()];
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("192.168.1.0/24"));
        assert!(rules.contains("tcp dport 9117 accept"));
    }

    #[test]
    fn ingress_ports_configurable() {
        let mut config = default_config();
        config.ingress_ports = vec![8080, 8443];
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("tcp dport 8080 accept"));
        assert!(rules.contains("tcp dport 8443 accept"));
        assert!(!rules.contains("tcp dport 80 accept"));
    }

    #[test]
    fn icmp_allowed() {
        let config = default_config();
        let nodes = ClusterNodes::new();
        let rules = generate_ruleset(&config, &nodes);

        assert!(rules.contains("icmp accept"));
    }

    #[test]
    fn management_port_from_cluster_nodes() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.5"]);
        let rules = generate_ruleset(&config, &nodes);

        // Cluster nodes should be able to reach management port
        assert!(rules.contains("10.0.1.5"));
        assert!(rules.contains(&format!("tcp dport {}", config.management_port)));
    }

    #[test]
    fn default_config_has_standard_ports() {
        let config = default_config();
        assert_eq!(config.ingress_ports, vec![80, 443]);
        assert_eq!(config.cluster_ports, vec![9443, 9444, 9445]);
        assert_eq!(config.management_port, 9117);
    }

    #[test]
    fn ruleset_is_valid_nft_structure() {
        let config = default_config();
        let nodes = cluster_with_nodes(&["10.0.1.1"]);
        let rules = generate_ruleset(&config, &nodes);

        // Should have balanced braces
        let open = rules.matches('{').count();
        let close = rules.matches('}').count();
        assert_eq!(open, close, "unbalanced braces in ruleset");

        // Should start with table and end with closing brace
        assert!(rules.starts_with("table ip reliaburger"));
        assert!(rules.trim().ends_with('}'));
    }
}

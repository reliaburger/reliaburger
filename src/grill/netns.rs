/// Per-container network namespace management (Linux only).
///
/// Creates an isolated network namespace for each container with a veth
/// pair connecting it to the host. This gives every container its own
/// IP address and network stack, which is the foundation for service
/// discovery (Onion) and ingress (Wrapper).
///
/// IP scheme: each node gets a `/23` subnet from the `10.0.0.0/8`
/// private range. A /23 gives 510 usable host addresses (enough for
/// 500 containers per node), and 10.0.0.0/8 has room for 32,768
/// /23 blocks (enough for 10k+ nodes).
///
/// Node N's subnet: `10.{(N*2) >> 8}.{(N*2) & 0xFF}.0/23`
/// Gateway: first usable IP in the /23 block
/// Containers: start at gateway + 1
///
/// Two port mapping strategies:
/// - **Root mode**: nftables DNAT rules (kernel-level, zero overhead).
/// - **Rootless mode**: Rust TCP proxy via tokio (userspace forwarding).
use std::net::Ipv4Addr;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::InstanceId;

/// Errors from network namespace operations.
#[derive(Debug, thiserror::Error)]
pub enum NetnsError {
    #[error("failed to create network namespace for {instance}: {reason}")]
    SetupFailed { instance: String, reason: String },

    #[error("failed to add port mapping {host_port}->{container_port} for {instance}: {reason}")]
    PortMappingFailed {
        instance: String,
        host_port: u16,
        container_port: u16,
        reason: String,
    },

    #[error("failed to tear down network for {instance}: {reason}")]
    TeardownFailed { instance: String, reason: String },
}

/// Tracks the network resources allocated to a single container.
///
/// Created by `setup_container_network`, cleaned up by
/// `teardown_container_network`. Holds everything needed to route
/// traffic to/from the container's isolated network namespace.
pub struct ContainerNetwork {
    /// Path to the network namespace file, e.g. `/var/run/netns/{instance_id}`.
    pub namespace_path: PathBuf,
    /// The container's IP address inside its namespace.
    pub container_ip: Ipv4Addr,
    /// The gateway IP (host-side veth endpoint).
    pub gateway_ip: Ipv4Addr,
    /// Name of the host-side veth interface.
    pub host_veth: String,
    /// Name of the container-side veth interface (always `eth0`).
    pub container_veth: String,
    /// Whether this network uses rootless mode (Rust proxy vs nftables).
    pub rootless: bool,
}

/// Handle to an active port mapping. Drop or call `shutdown()` to
/// remove the mapping.
pub struct PortMapHandle {
    /// Cancellation token that stops the TCP proxy (rootless) or
    /// signals that the nftables rule should be removed (root).
    shutdown: CancellationToken,
    /// The nftables rule spec, if root mode. Used for cleanup.
    nft_rule: Option<NftRule>,
    /// Host port being mapped.
    pub host_port: u16,
    /// Container port being mapped.
    pub container_port: u16,
}

/// Stored nftables rule details for cleanup.
struct NftRule {
    host_port: u16,
    container_ip: Ipv4Addr,
    container_port: u16,
}

impl PortMapHandle {
    /// Shut down this port mapping. For rootless mode, cancels the TCP
    /// proxy task. For root mode, removes the nftables rule.
    pub async fn shutdown(self) -> Result<(), NetnsError> {
        self.shutdown.cancel();

        if let Some(rule) = &self.nft_rule {
            remove_nft_rule(rule).await?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IP address calculation
// ---------------------------------------------------------------------------

/// Maximum containers per node. A /23 subnet has 510 usable addresses;
/// one is the gateway, leaving 509 for containers.
pub const MAX_CONTAINERS_PER_NODE: u16 = 509;

/// Maximum node index. 10.0.0.0/8 has 2^24 addresses, each /23 uses
/// 512, so we get 2^24 / 2^9 = 32,768 possible /23 blocks. Index 0
/// is reserved, giving us 1..32,767.
pub const MAX_NODE_INDEX: u16 = 32_767;

/// Compute the base address for a node's /23 subnet.
///
/// Node N's block starts at `10.{(N*2) >> 8}.{(N*2) & 0xFF}.0`.
/// Each node consumes two consecutive /24 blocks (a /23).
fn subnet_base(node_index: u16) -> (u8, u8) {
    let offset = (node_index as u32) * 2;
    let second_octet = (offset >> 8) as u8;
    let third_octet = (offset & 0xFF) as u8;
    (second_octet, third_octet)
}

/// Compute the container's IP address from the node index and
/// container index within that node.
///
/// Container index 0 maps to the second usable address in the /23
/// block (the first is the gateway).
pub fn container_ip(node_index: u16, container_index: u16) -> Ipv4Addr {
    let (oct2, oct3) = subnet_base(node_index);
    // Gateway is at base + 1 (i.e. .0.1), containers start at base + 2.
    // A /23 spans two /24 blocks, so the fourth octet wraps from the
    // lower block (.0.x) into the upper block (.1.x).
    let host_offset = (container_index as u32) + 2;
    let third = oct3.wrapping_add((host_offset >> 8) as u8);
    let fourth = (host_offset & 0xFF) as u8;
    Ipv4Addr::new(10, oct2, third, fourth)
}

/// Compute the gateway IP for a node's /23 subnet.
///
/// The gateway is the first usable address: `10.{oct2}.{oct3}.1`.
pub fn gateway_ip(node_index: u16) -> Ipv4Addr {
    let (oct2, oct3) = subnet_base(node_index);
    Ipv4Addr::new(10, oct2, oct3, 1)
}

/// Compute the CIDR string for a node's /23 subnet.
pub fn subnet_cidr(node_index: u16) -> String {
    let (oct2, oct3) = subnet_base(node_index);
    format!("10.{oct2}.{oct3}.0/23")
}

/// Derive a node index from a node ID string. Uses djb2 hash to map
/// the node ID into the 1..32,767 range.
pub fn node_index_from_id(node_id: &str) -> u16 {
    let hash: u32 = node_id.bytes().fold(5381u32, |acc, b| {
        acc.wrapping_mul(33).wrapping_add(b as u32)
    });
    ((hash % (MAX_NODE_INDEX as u32)) + 1) as u16
}

/// Generate the namespace path for an instance.
pub fn namespace_path(instance_id: &InstanceId) -> PathBuf {
    PathBuf::from(format!("/var/run/netns/rb-{}", instance_id.0))
}

/// Generate the host-side veth name for an instance.
///
/// Truncated to 15 characters (Linux interface name limit).
pub fn host_veth_name(instance_id: &InstanceId) -> String {
    let raw = format!("veth-{}-h", instance_id.0);
    if raw.len() > 15 {
        raw[..15].to_string()
    } else {
        raw
    }
}

// ---------------------------------------------------------------------------
// Namespace lifecycle
// ---------------------------------------------------------------------------

/// Create a network namespace, veth pair, assign IPs, and set the
/// default route inside the namespace.
///
/// This is the main entry point for setting up container networking.
/// After this returns, the container can be started with its network
/// namespace path in the OCI spec, and it will have connectivity to
/// the host via the veth pair.
pub async fn setup_container_network(
    instance_id: &InstanceId,
    node_index: u16,
    container_index: u16,
    rootless: bool,
) -> Result<ContainerNetwork, NetnsError> {
    let ns_path = namespace_path(instance_id);
    let c_ip = container_ip(node_index, container_index);
    let gw_ip = gateway_ip(node_index);
    let h_veth = host_veth_name(instance_id);
    // Use a unique container-side veth name. We use the last 13 chars
    // of a hash to avoid truncation collisions with the host-side name.
    let c_hash = {
        let h: u32 = instance_id
            .0
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        format!("c-{h:08x}")
    };
    let c_veth_tmp = format!("vt{c_hash}");
    // vt + c- + 8 hex = 12 chars, well under the 15-char limit
    let ns_name = format!("rb-{}", instance_id.0);

    // 1. Create the network namespace
    run_cmd(
        "ip",
        &["netns", "add", &ns_name],
        instance_id,
        "create network namespace",
    )
    .await?;

    // 2. Create the veth pair (using a temporary name for the container side)
    run_cmd(
        "ip",
        &[
            "link",
            "add",
            &h_veth,
            "type",
            "veth",
            "peer",
            "name",
            &c_veth_tmp,
        ],
        instance_id,
        "create veth pair",
    )
    .await?;

    // 3. Move the container-side veth into the namespace
    run_cmd(
        "ip",
        &["link", "set", &c_veth_tmp, "netns", &ns_name],
        instance_id,
        "move veth to namespace",
    )
    .await?;

    // 3b. Rename to eth0 inside the namespace
    let c_veth = "eth0".to_string();
    run_cmd(
        "ip",
        &[
            "netns",
            "exec",
            &ns_name,
            "ip",
            "link",
            "set",
            &c_veth_tmp,
            "name",
            &c_veth,
        ],
        instance_id,
        "rename veth to eth0 in namespace",
    )
    .await?;

    // 4. Assign IP to host-side veth
    let gw_cidr = format!("{gw_ip}/23");
    run_cmd(
        "ip",
        &["addr", "add", &gw_cidr, "dev", &h_veth],
        instance_id,
        "assign IP to host veth",
    )
    .await?;

    // 5. Bring up host-side veth
    run_cmd(
        "ip",
        &["link", "set", &h_veth, "up"],
        instance_id,
        "bring up host veth",
    )
    .await?;

    // 6. Assign IP to container-side veth (inside namespace)
    let c_cidr = format!("{c_ip}/23");
    run_cmd(
        "ip",
        &[
            "netns", "exec", &ns_name, "ip", "addr", "add", &c_cidr, "dev", &c_veth,
        ],
        instance_id,
        "assign IP to container veth",
    )
    .await?;

    // 7. Bring up container-side veth
    run_cmd(
        "ip",
        &[
            "netns", "exec", &ns_name, "ip", "link", "set", &c_veth, "up",
        ],
        instance_id,
        "bring up container veth",
    )
    .await?;

    // 8. Bring up loopback inside namespace
    run_cmd(
        "ip",
        &["netns", "exec", &ns_name, "ip", "link", "set", "lo", "up"],
        instance_id,
        "bring up loopback in namespace",
    )
    .await?;

    // 9. Set default route inside namespace
    let gw_str = gw_ip.to_string();
    run_cmd(
        "ip",
        &[
            "netns", "exec", &ns_name, "ip", "route", "add", "default", "via", &gw_str,
        ],
        instance_id,
        "set default route in namespace",
    )
    .await?;

    // 10. Enable IP forwarding on the host (idempotent)
    run_cmd(
        "sysctl",
        &["-w", "net.ipv4.ip_forward=1"],
        instance_id,
        "enable IP forwarding",
    )
    .await?;

    Ok(ContainerNetwork {
        namespace_path: ns_path,
        container_ip: c_ip,
        gateway_ip: gw_ip,
        host_veth: h_veth,
        container_veth: c_veth,
        rootless,
    })
}

/// Add a port mapping from a host port to a container port.
///
/// In root mode, adds an nftables DNAT rule. In rootless mode, spawns
/// a tokio TCP proxy task.
pub async fn add_port_mapping(
    network: &ContainerNetwork,
    host_port: u16,
    container_port: u16,
) -> Result<PortMapHandle, NetnsError> {
    let shutdown = CancellationToken::new();

    if network.rootless {
        // Rootless: spawn a TCP proxy
        let container_ip = network.container_ip;
        let token = shutdown.clone();

        tokio::spawn(async move {
            if let Err(e) = run_tcp_proxy(host_port, container_ip, container_port, token).await {
                eprintln!("tcp proxy error for port {host_port}: {e}");
            }
        });

        Ok(PortMapHandle {
            shutdown,
            nft_rule: None,
            host_port,
            container_port,
        })
    } else {
        // Root: nftables DNAT
        ensure_nft_table()
            .await
            .map_err(|e| NetnsError::PortMappingFailed {
                instance: "nft-setup".to_string(),
                host_port,
                container_port,
                reason: e,
            })?;

        let rule_spec = format!(
            "tcp dport {} dnat to {}:{}",
            host_port, network.container_ip, container_port
        );
        run_cmd_raw(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                "reliaburger",
                "prerouting",
                "tcp",
                "dport",
                &host_port.to_string(),
                "dnat",
                "to",
                &format!("{}:{}", network.container_ip, container_port),
            ],
            &format!("add nft rule: {rule_spec}"),
        )
        .await
        .map_err(|e| NetnsError::PortMappingFailed {
            instance: "nft".to_string(),
            host_port,
            container_port,
            reason: e,
        })?;

        Ok(PortMapHandle {
            shutdown,
            nft_rule: Some(NftRule {
                host_port,
                container_ip: network.container_ip,
                container_port,
            }),
            host_port,
            container_port,
        })
    }
}

/// Remove a network namespace, veth pair, and clean up.
pub async fn teardown_container_network(network: &ContainerNetwork) -> Result<(), NetnsError> {
    let ns_name = network
        .namespace_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // Delete the host-side veth (this also removes the peer)
    let _ = run_cmd_raw(
        "ip",
        &["link", "del", &network.host_veth],
        "delete host veth",
    )
    .await;

    // Delete the network namespace
    let _ = run_cmd_raw("ip", &["netns", "del", ns_name], "delete network namespace").await;

    Ok(())
}

// ---------------------------------------------------------------------------
// nftables helpers
// ---------------------------------------------------------------------------

/// Ensure the reliaburger nftables table and prerouting chain exist.
async fn ensure_nft_table() -> Result<(), String> {
    // Create table (idempotent — nft doesn't error if it exists)
    run_cmd_raw(
        "nft",
        &["add", "table", "ip", "reliaburger"],
        "create nft table",
    )
    .await?;

    // Create prerouting chain
    run_cmd_raw(
        "nft",
        &[
            "add",
            "chain",
            "ip",
            "reliaburger",
            "prerouting",
            "{ type nat hook prerouting priority -100 ; }",
        ],
        "create nft prerouting chain",
    )
    .await?;

    // Create postrouting chain for masquerade
    run_cmd_raw(
        "nft",
        &[
            "add",
            "chain",
            "ip",
            "reliaburger",
            "postrouting",
            "{ type nat hook postrouting priority 100 ; }",
        ],
        "create nft postrouting chain",
    )
    .await?;

    // Masquerade outgoing traffic from container subnets
    run_cmd_raw(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            "reliaburger",
            "postrouting",
            "ip",
            "saddr",
            "10.0.0.0/8",
            "masquerade",
        ],
        "add masquerade rule",
    )
    .await?;

    Ok(())
}

/// Remove an nftables DNAT rule.
async fn remove_nft_rule(rule: &NftRule) -> Result<(), NetnsError> {
    // List rules to find the handle, then delete by handle.
    // This is simpler than tracking rule handles at creation time.
    let output = tokio::process::Command::new("nft")
        .args(["-a", "list", "chain", "ip", "reliaburger", "prerouting"])
        .output()
        .await
        .map_err(|e| NetnsError::TeardownFailed {
            instance: "nft".to_string(),
            reason: format!("failed to list nft rules: {e}"),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let search = format!("dnat to {}:{}", rule.container_ip, rule.container_port);

    for line in stdout.lines() {
        if line.contains(&search) && line.contains(&format!("dport {}", rule.host_port)) {
            // Extract handle number from "# handle N"
            if let Some(handle) = line
                .rsplit("# handle ")
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                let _ = run_cmd_raw(
                    "nft",
                    &[
                        "delete",
                        "rule",
                        "ip",
                        "reliaburger",
                        "prerouting",
                        "handle",
                        &handle.to_string(),
                    ],
                    "delete nft rule",
                )
                .await;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// TCP proxy (rootless mode)
// ---------------------------------------------------------------------------

/// Run a TCP proxy that forwards connections from `host_port` to
/// `container_ip:container_port`. Runs until the cancellation token
/// is triggered.
async fn run_tcp_proxy(
    host_port: u16,
    container_ip: Ipv4Addr,
    container_port: u16,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", host_port)).await?;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accept = listener.accept() => {
                let (client, _addr) = accept?;
                let target_addr = std::net::SocketAddr::new(
                    std::net::IpAddr::V4(container_ip),
                    container_port,
                );
                let token = shutdown.clone();

                tokio::spawn(async move {
                    let upstream = match tokio::net::TcpStream::connect(target_addr).await {
                        Ok(s) => s,
                        Err(_) => return,
                    };

                    let (mut client_read, mut client_write) = tokio::io::split(client);
                    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);

                    tokio::select! {
                        _ = token.cancelled() => {},
                        _ = async {
                            let c2u = tokio::io::copy(&mut client_read, &mut upstream_write);
                            let u2c = tokio::io::copy(&mut upstream_read, &mut client_write);
                            let _ = tokio::try_join!(c2u, u2c);
                        } => {},
                    }
                });
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Command helpers
// ---------------------------------------------------------------------------

/// Run a command, returning a `NetnsError` on failure.
async fn run_cmd(
    program: &str,
    args: &[&str],
    instance_id: &InstanceId,
    description: &str,
) -> Result<(), NetnsError> {
    run_cmd_raw(program, args, description)
        .await
        .map_err(|reason| NetnsError::SetupFailed {
            instance: instance_id.0.clone(),
            reason,
        })
}

/// Run a command, returning a descriptive error string on failure.
async fn run_cmd_raw(program: &str, args: &[&str], description: &str) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{description}: failed to execute {program}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{description}: {program} failed: {stderr}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// nftables rule generation (for testing)
// ---------------------------------------------------------------------------

/// Generate the nftables DNAT rule string for a port mapping.
///
/// This is a pure function used in tests to verify rule formatting
/// without needing root or nftables installed.
pub fn nft_dnat_rule(host_port: u16, container_ip: Ipv4Addr, container_port: u16) -> String {
    format!("tcp dport {host_port} dnat to {container_ip}:{container_port}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- IP address calculation -----------------------------------------------

    #[test]
    fn container_ip_first_container_node_1() {
        // Node 1: subnet base = 10.0.2.0/23, gateway = .2.1, first container = .2.2
        let ip = container_ip(1, 0);
        assert_eq!(ip, Ipv4Addr::new(10, 0, 2, 2));
    }

    #[test]
    fn container_ip_tenth_container() {
        let ip = container_ip(1, 9);
        assert_eq!(ip, Ipv4Addr::new(10, 0, 2, 11));
    }

    #[test]
    fn container_ip_wraps_into_upper_block() {
        // Container index 253 → offset 255. That's .2.255 (still in lower /24 of the /23).
        // Container index 254 → offset 256 → wraps to .3.0
        let ip = container_ip(1, 254);
        assert_eq!(ip, Ipv4Addr::new(10, 0, 3, 0));
    }

    #[test]
    fn container_ip_max_per_node() {
        // Container index 508 (the 509th container, our MAX_CONTAINERS_PER_NODE)
        // offset = 510 = 0x1FE → .3.254
        let ip = container_ip(1, 508);
        assert_eq!(ip, Ipv4Addr::new(10, 0, 3, 254));
    }

    #[test]
    fn gateway_ip_node_0() {
        // Node 0: subnet base = 10.0.0.0/23, gateway = .0.1
        assert_eq!(gateway_ip(0), Ipv4Addr::new(10, 0, 0, 1));
    }

    #[test]
    fn gateway_ip_node_1() {
        // Node 1: subnet base = 10.0.2.0/23, gateway = .2.1
        assert_eq!(gateway_ip(1), Ipv4Addr::new(10, 0, 2, 1));
    }

    #[test]
    fn gateway_ip_node_128() {
        // Node 128: offset = 256 → 10.1.0.0/23, gateway = 10.1.0.1
        assert_eq!(gateway_ip(128), Ipv4Addr::new(10, 1, 0, 1));
    }

    #[test]
    fn gateway_ip_node_5000() {
        // Node 5000: offset = 10000 → 10.39.16.0/23, gateway = 10.39.16.1
        assert_eq!(gateway_ip(5000), Ipv4Addr::new(10, 39, 16, 1));
    }

    #[test]
    fn subnet_cidr_format() {
        assert_eq!(subnet_cidr(0), "10.0.0.0/23");
        assert_eq!(subnet_cidr(1), "10.0.2.0/23");
        assert_eq!(subnet_cidr(128), "10.1.0.0/23");
    }

    #[test]
    fn ten_thousand_nodes_fit() {
        // Verify that node 10,000 produces a valid 10.x.x.x address
        let gw = gateway_ip(10_000);
        assert_eq!(gw.octets()[0], 10);
        // 10,000 * 2 = 20,000 → 10.78.32.1
        assert_eq!(gw, Ipv4Addr::new(10, 78, 32, 1));
    }

    #[test]
    fn five_hundred_containers_fit() {
        // Verify container index 499 (the 500th container) works
        let ip = container_ip(1, 499);
        // offset = 501 = 0x1F5 → third_octet = 2 + 1 = 3, fourth = 0xF5 = 245
        assert_eq!(ip, Ipv4Addr::new(10, 0, 3, 245));
    }

    // -- Node index from ID ---------------------------------------------------

    #[test]
    fn node_index_in_valid_range() {
        for id in &["node-1", "node-2", "worker-alpha", "srv-prod-03"] {
            let idx = node_index_from_id(id);
            assert!(
                idx >= 1 && idx <= MAX_NODE_INDEX,
                "index {idx} out of range for {id}"
            );
        }
    }

    #[test]
    fn node_index_deterministic() {
        let a = node_index_from_id("node-1");
        let b = node_index_from_id("node-1");
        assert_eq!(a, b);
    }

    #[test]
    fn node_index_different_ids_likely_different() {
        let a = node_index_from_id("node-1");
        let b = node_index_from_id("node-2");
        assert_ne!(a, b);
    }

    // -- Namespace path generation --------------------------------------------

    #[test]
    fn namespace_path_format() {
        let id = InstanceId("web-0".to_string());
        assert_eq!(
            namespace_path(&id),
            PathBuf::from("/var/run/netns/rb-web-0")
        );
    }

    // -- Host veth name -------------------------------------------------------

    #[test]
    fn host_veth_name_short_instance() {
        let id = InstanceId("web-0".to_string());
        assert_eq!(host_veth_name(&id), "veth-web-0-h");
    }

    #[test]
    fn host_veth_name_truncated_to_15_chars() {
        let id = InstanceId("very-long-instance-name-42".to_string());
        let name = host_veth_name(&id);
        assert!(name.len() <= 15, "veth name too long: {name}");
    }

    // -- nftables rule generation ---------------------------------------------

    #[test]
    fn nft_dnat_rule_format() {
        let rule = nft_dnat_rule(8080, Ipv4Addr::new(10, 0, 2, 2), 80);
        assert_eq!(rule, "tcp dport 8080 dnat to 10.0.2.2:80");
    }

    #[test]
    fn nft_dnat_rule_high_port() {
        let rule = nft_dnat_rule(30000, Ipv4Addr::new(10, 39, 16, 11), 3000);
        assert_eq!(rule, "tcp dport 30000 dnat to 10.39.16.11:3000");
    }

    // -- Integration tests (Linux only, root required) ------------------------

    fn netns_tests_enabled() -> bool {
        std::env::var("RELIABURGER_NETNS_TESTS").is_ok()
    }

    #[tokio::test]
    async fn setup_and_teardown_container_network() {
        if !netns_tests_enabled() {
            eprintln!("skipping netns test (set RELIABURGER_NETNS_TESTS=1 to enable)");
            return;
        }

        let id = InstanceId("netns-test-0".to_string());
        // Clean up any leftovers from a previous failed run
        let _ = run_cmd_raw(
            "ip",
            &["link", "del", &host_veth_name(&id)],
            "pre-cleanup veth",
        )
        .await;
        let _ = run_cmd_raw(
            "ip",
            &["netns", "del", "rb-netns-test-0"],
            "pre-cleanup netns",
        )
        .await;

        // Use node_index 99: subnet base = 10.0.198.0/23
        let network = setup_container_network(&id, 99, 0, false)
            .await
            .expect("failed to set up container network");

        let expected_gw = gateway_ip(99);
        let expected_ip = container_ip(99, 0);
        assert_eq!(network.container_ip, expected_ip);
        assert_eq!(network.gateway_ip, expected_gw);
        assert!(network.namespace_path.exists());

        // Verify the container can ping the gateway
        let gw_str = expected_gw.to_string();
        let ping = tokio::process::Command::new("ip")
            .args([
                "netns",
                "exec",
                "rb-netns-test-0",
                "ping",
                "-c",
                "1",
                "-W",
                "2",
                &gw_str,
            ])
            .output()
            .await
            .expect("ping command failed");
        assert!(ping.status.success(), "container cannot ping gateway");

        // Clean up
        teardown_container_network(&network)
            .await
            .expect("failed to tear down");
    }

    #[tokio::test]
    async fn port_mapping_nftables() {
        if !netns_tests_enabled() {
            eprintln!("skipping netns test (set RELIABURGER_NETNS_TESTS=1 to enable)");
            return;
        }

        let id = InstanceId("netns-portmap-0".to_string());
        // Clean up any leftovers from a previous failed run
        let _ = run_cmd_raw(
            "ip",
            &["link", "del", &host_veth_name(&id)],
            "pre-cleanup veth",
        )
        .await;
        let _ = run_cmd_raw(
            "ip",
            &["netns", "del", "rb-netns-portmap-0"],
            "pre-cleanup netns",
        )
        .await;

        let network = setup_container_network(&id, 98, 0, false)
            .await
            .expect("failed to set up container network");

        let handle = add_port_mapping(&network, 18080, 80)
            .await
            .expect("failed to add port mapping");

        assert_eq!(handle.host_port, 18080);
        assert_eq!(handle.container_port, 80);

        // Clean up
        handle
            .shutdown()
            .await
            .expect("failed to remove port mapping");
        teardown_container_network(&network)
            .await
            .expect("failed to tear down");
    }
}

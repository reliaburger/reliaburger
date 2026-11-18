/// The `reliaburger_isolation` table: namespace isolation between local
/// workloads on the forward path, for traffic the eBPF socket hooks never see.
pub mod isolation;
/// nftables perimeter firewall.
///
/// Blocks external access to Reliaburger's own ports: container host
/// ports (`[network] port_range`), cluster ports (gossip, Raft,
/// reporting) and the management API. Cluster nodes bypass every block;
/// listed bootstrap peers reach the management and cluster ports; operator
/// CIDRs (`[security] operator_cidrs`) reach the management API port only.
/// Everything else (SSH, operator services) is untouched. Published container
/// ports are held to the same policy on the forward hook: the port map
/// rewrites them to a container address, so they never meet the input chain.
/// That chain matches the rewrite, not a port, so a port Bun serves on stays
/// reachable even inside the host-port range.
///
/// Uses its own `reliaburger_fw` nftables table, separate from the
/// `reliaburger` table `grill::netns` uses for port mapping. Rules are
/// reconciled at startup and whenever cluster membership changes.
///
/// Linux only. On macOS, the firewall module compiles but all
/// operations are no-ops.
pub mod rules;

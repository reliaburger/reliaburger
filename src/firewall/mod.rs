/// nftables perimeter firewall.
///
/// Blocks external access to Reliaburger's own ports: container
/// host ports (30000-31000), cluster ports (gossip, Raft, reporting),
/// and the management API. Cluster nodes and admin CIDRs bypass
/// the blocks. Everything else (SSH, operator services) is untouched.
///
/// Uses the `reliaburger` nftables table (shared with netns port
/// mapping). Rules are reconciled on cluster membership changes
/// and at a 30-second interval.
///
/// Linux only. On macOS, the firewall module compiles but all
/// operations are no-ops.
pub mod rules;

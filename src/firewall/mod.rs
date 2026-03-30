/// nftables perimeter firewall.
///
/// Enforces cluster boundary rules: only ingress ports (80/443),
/// inter-node cluster traffic (gossip, Raft, reporting), and
/// management access (admin CIDRs) are allowed. Everything else
/// is dropped.
///
/// Uses the `reliaburger` nftables table (shared with netns port
/// mapping). Rules are reconciled on cluster membership changes
/// and at a 30-second interval.
///
/// Linux only. On macOS, the firewall module compiles but all
/// operations are no-ops.
pub mod rules;

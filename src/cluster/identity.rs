//! Stable mapping between gossip node names and Raft numeric ids/addresses.
//!
//! Gossip identifies nodes by name (`NodeId(String)`); openraft identifies
//! them by `u64`. We derive the `u64` deterministically from the name so a
//! node keeps the same Raft id across restarts, and we map a node's gossip
//! address to its Raft RPC address (same IP, the configured `raft_port`).

use std::net::SocketAddr;

use crate::council::types::CouncilNodeInfo;
use crate::mustard::membership::MembershipSnapshot;

/// Derive a stable `u64` Raft id from a node name using djb2 (the same hash
/// the networking layer uses for node indices). Deterministic across
/// restarts. Never returns 0, which openraft treats specially in some paths.
pub fn raft_id_from_name(name: &str) -> u64 {
    let hash = name.bytes().fold(5381u64, |acc, b| {
        acc.wrapping_mul(33).wrapping_add(b as u64)
    });
    if hash == 0 { 1 } else { hash }
}

/// Build the `(raft_id, CouncilNodeInfo)` pair for a gossiped node, mapping
/// its gossip address to its Raft RPC address (same IP, `raft_port`).
pub fn council_info(snapshot: &MembershipSnapshot, raft_port: u16) -> (u64, CouncilNodeInfo) {
    let raft_addr = SocketAddr::new(snapshot.address.ip(), raft_port);
    (
        raft_id_from_name(&snapshot.node_id.0),
        CouncilNodeInfo {
            addr: raft_addr,
            name: snapshot.node_id.0.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raft_id_is_deterministic_and_nonzero() {
        assert_eq!(raft_id_from_name("node-1"), raft_id_from_name("node-1"));
        assert_ne!(raft_id_from_name("node-1"), raft_id_from_name("node-2"));
        assert_ne!(raft_id_from_name(""), 0);
    }

    #[test]
    fn council_info_maps_gossip_addr_to_raft_port() {
        use crate::meat::types::NodeId;
        use crate::mustard::state::NodeState;
        use std::collections::BTreeMap;

        let snap = MembershipSnapshot {
            node_id: NodeId::new("web-3"),
            address: "10.0.2.5:9443".parse().unwrap(),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
        };
        let (id, info) = council_info(&snap, 9444);
        assert_eq!(id, raft_id_from_name("web-3"));
        assert_eq!(info.name, "web-3");
        assert_eq!(info.addr, "10.0.2.5:9444".parse().unwrap());
    }
}

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

/// Build the `(raft_id, CouncilNodeInfo)` pair for a gossiped node.
///
/// The peer's Raft RPC address is its gossip IP with a Raft port derived from
/// its *own* advertised gossip port plus `port_offset` (the local config's
/// `raft_port - gossip_port`). Deriving per-peer — rather than assuming one
/// fixed Raft port for everyone — is essential: with several nodes on one host
/// (distinct ports, shared IP) a fixed port would collide every peer onto the
/// same address. Gossip already propagates each node's full address, and a
/// homogeneous cluster shares the same offset, so this is correct whether
/// nodes are distinguished by IP or by port.
pub fn council_info(snapshot: &MembershipSnapshot, port_offset: i32) -> (u64, CouncilNodeInfo) {
    let raft_port = (snapshot.address.port() as i32 + port_offset) as u16;
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
        // Offset of 1 (gossip 9443 -> raft 9444), derived from the peer's
        // own gossip port.
        let (id, info) = council_info(&snap, 1);
        assert_eq!(id, raft_id_from_name("web-3"));
        assert_eq!(info.name, "web-3");
        assert_eq!(info.addr, "10.0.2.5:9444".parse().unwrap());
    }

    #[test]
    fn council_info_derives_distinct_raft_addrs_for_same_ip() {
        use crate::meat::types::NodeId;
        use crate::mustard::state::NodeState;
        use std::collections::BTreeMap;

        // Two nodes on the same host, distinguished by port. With offset 1,
        // each gets a distinct raft addr (no collision) — the bug we fixed.
        let mk = |port: u16, name: &str| MembershipSnapshot {
            node_id: NodeId::new(name),
            address: SocketAddr::from(([127, 0, 0, 1], port)),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
        };
        let (_, a) = council_info(&mk(17541, "c1"), 1);
        let (_, b) = council_info(&mk(17543, "c2"), 1);
        assert_eq!(a.addr, "127.0.0.1:17542".parse().unwrap());
        assert_eq!(b.addr, "127.0.0.1:17544".parse().unwrap());
        assert_ne!(a.addr, b.addr);
    }
}

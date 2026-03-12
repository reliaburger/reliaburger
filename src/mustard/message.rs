/// Gossip message types.
///
/// All messages are fixed-size to keep UDP datagrams under the MTU
/// (max 1400 bytes). Membership updates are piggybacked on every
/// PING/ACK exchange, achieving O(log N) convergence without
/// dedicated broadcast messages.
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::patty::NodeId;

use super::state::NodeState;

/// Maximum number of piggybacked membership updates per gossip message.
/// Bounded to keep message size constant (~512 bytes total).
pub const MAX_PIGGYBACK_UPDATES: usize = 8;

/// Top-level gossip message sent as a single UDP datagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Protocol version for forward compatibility.
    pub version: u8,
    /// Sender's node identity.
    pub sender: NodeId,
    /// Sender's current incarnation number.
    pub incarnation: u64,
    /// HMAC-SHA256 of the serialised payload (zeroed until Phase 4 adds mTLS).
    pub hmac: [u8; 32],
    /// The message payload.
    pub payload: GossipPayload,
}

impl GossipMessage {
    /// Current protocol version.
    pub const VERSION: u8 = 1;

    /// Create a new gossip message with the given sender and payload.
    /// HMAC is zeroed; Phase 4 will populate it.
    pub fn new(sender: NodeId, incarnation: u64, payload: GossipPayload) -> Self {
        Self {
            version: Self::VERSION,
            sender,
            incarnation,
            hmac: [0u8; 32],
            payload,
        }
    }
}

/// The payload of a gossip message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipPayload {
    /// Direct probe: "are you alive?"
    Ping {
        /// Piggybacked membership updates.
        updates: Vec<MembershipUpdate>,
    },
    /// Indirect probe request: "please probe this target for me."
    PingReq {
        /// The node to probe.
        target: NodeId,
        /// Who asked for the indirect probe (so the ACK can be routed back).
        requester: NodeId,
        /// Piggybacked membership updates.
        updates: Vec<MembershipUpdate>,
    },
    /// Response to a PING or forwarded PING-REQ.
    Ack {
        /// Piggybacked membership updates.
        updates: Vec<MembershipUpdate>,
    },
}

impl GossipPayload {
    /// Extract the piggybacked updates from any payload variant.
    pub fn updates(&self) -> &[MembershipUpdate] {
        match self {
            GossipPayload::Ping { updates }
            | GossipPayload::PingReq { updates, .. }
            | GossipPayload::Ack { updates } => updates,
        }
    }
}

/// A single membership update piggybacked on gossip messages.
///
/// Carries the node's identity, its new state, the incarnation number
/// for conflict resolution, and a Lamport timestamp for causal ordering.
/// The address is included so that nodes learning about a peer through
/// gossip (not direct contact) can reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipUpdate {
    /// Which node this update concerns.
    pub node_id: NodeId,
    /// The node's cluster address.
    pub address: SocketAddr,
    /// The reported state of the node.
    pub state: NodeState,
    /// Incarnation number for CRDT-like conflict resolution.
    pub incarnation: u64,
    /// Lamport timestamp for ordering.
    pub lamport: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossip_message_new_sets_version_and_zeroed_hmac() {
        let msg = GossipMessage::new(
            NodeId::new("node-1"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        assert_eq!(msg.version, GossipMessage::VERSION);
        assert_eq!(msg.hmac, [0u8; 32]);
        assert_eq!(msg.sender, NodeId::new("node-1"));
        assert_eq!(msg.incarnation, 1);
    }

    fn test_addr() -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], 9000))
    }

    #[test]
    fn gossip_payload_updates_extracts_from_ping() {
        let updates = vec![MembershipUpdate {
            node_id: NodeId::new("node-2"),
            address: test_addr(),
            state: NodeState::Alive,
            incarnation: 1,
            lamport: 1,
        }];
        let payload = GossipPayload::Ping {
            updates: updates.clone(),
        };
        assert_eq!(payload.updates(), &updates);
    }

    #[test]
    fn gossip_payload_updates_extracts_from_ping_req() {
        let updates = vec![MembershipUpdate {
            node_id: NodeId::new("node-3"),
            address: test_addr(),
            state: NodeState::Suspect,
            incarnation: 2,
            lamport: 5,
        }];
        let payload = GossipPayload::PingReq {
            target: NodeId::new("node-2"),
            requester: NodeId::new("node-1"),
            updates: updates.clone(),
        };
        assert_eq!(payload.updates(), &updates);
    }

    #[test]
    fn gossip_payload_updates_extracts_from_ack() {
        let payload = GossipPayload::Ack { updates: vec![] };
        assert!(payload.updates().is_empty());
    }

    #[test]
    fn membership_update_serialisation_round_trip() {
        let update = MembershipUpdate {
            node_id: NodeId::new("node-1"),
            address: test_addr(),
            state: NodeState::Suspect,
            incarnation: 42,
            lamport: 100,
        };
        let json = serde_json::to_string(&update).unwrap();
        let decoded: MembershipUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, decoded);
    }

    #[test]
    fn gossip_message_serialisation_round_trip() {
        let msg = GossipMessage::new(
            NodeId::new("sender"),
            5,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("target"),
                    address: test_addr(),
                    state: NodeState::Dead,
                    incarnation: 3,
                    lamport: 10,
                }],
            },
        );
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: GossipMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.version, msg.version);
        assert_eq!(decoded.sender, msg.sender);
        assert_eq!(decoded.incarnation, msg.incarnation);
    }
}

/// Gossip message types.
///
/// All messages are fixed-size to keep UDP datagrams under the MTU
/// (max 1400 bytes). Membership updates are piggybacked on every
/// PING/ACK exchange, achieving O(log N) convergence without
/// dedicated broadcast messages.
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::meat::NodeId;

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
    /// HMAC-SHA256 over the rest of the message. Zeroed at construction; the
    /// transport signs it on send and verifies it on receive when keyed.
    pub hmac: [u8; 32],
    /// The message payload.
    pub payload: GossipPayload,
}

impl GossipMessage {
    /// Current protocol version.
    pub const VERSION: u8 = 1;

    /// Create a new gossip message with the given sender and payload.
    /// HMAC is zeroed; the transport signs it on send when a key is configured.
    pub fn new(sender: NodeId, incarnation: u64, payload: GossipPayload) -> Self {
        Self {
            version: Self::VERSION,
            sender,
            incarnation,
            hmac: [0u8; 32],
            payload,
        }
    }

    /// The canonical bytes the HMAC is computed over: the whole message with
    /// `hmac` zeroed. Both sender and receiver compute this identically.
    ///
    /// This is deterministic only because the payload contains no unordered
    /// collections (`GossipPayload` is `Vec`s and scalars). **Any future map
    /// field must be an ordered map (`BTreeMap`)** — a `HashMap`'s bincode order
    /// varies per process and would silently break cross-node verification.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        let mut canonical = self.clone();
        canonical.hmac = [0u8; 32];
        bincode::serialize(&canonical)
    }

    /// Return a copy of this message with `hmac` set to the HMAC of its
    /// canonical bytes under `key`.
    pub fn signed(mut self, key: &ring::hmac::Key) -> Result<Self, bincode::Error> {
        self.hmac = [0u8; 32];
        let tag = crate::sesame::mtls::gossip_hmac::sign(key, &self.canonical_bytes()?);
        // HMAC-SHA256 is always 32 bytes.
        self.hmac.copy_from_slice(&tag);
        Ok(self)
    }

    /// Verify this message's `hmac` against its canonical bytes under `key`.
    /// Returns `false` on any serialisation error or tag mismatch.
    pub fn verify_hmac(&self, key: &ring::hmac::Key) -> bool {
        match self.canonical_bytes() {
            Ok(bytes) => crate::sesame::mtls::gossip_hmac::verify(key, &bytes, &self.hmac),
            Err(_) => false,
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

    fn a_message() -> GossipMessage {
        GossipMessage::new(
            NodeId::new("sender"),
            5,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("target"),
                    address: test_addr(),
                    state: NodeState::Alive,
                    incarnation: 3,
                    lamport: 10,
                }],
            },
        )
    }

    #[test]
    fn canonical_bytes_are_identical_regardless_of_hmac_contents() {
        let mut a = a_message();
        let mut b = a.clone();
        a.hmac = [1u8; 32];
        b.hmac = [2u8; 32];
        assert_eq!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
    }

    #[test]
    fn signed_message_verifies_with_the_same_key() {
        let key = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[3u8; 32]);
        let signed = a_message().signed(&key).unwrap();
        assert_ne!(signed.hmac, [0u8; 32]);
        assert!(signed.verify_hmac(&key));
    }

    #[test]
    fn signed_message_fails_verification_after_payload_is_mutated() {
        let key = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[3u8; 32]);
        let mut signed = a_message().signed(&key).unwrap();
        signed.incarnation += 1;
        assert!(!signed.verify_hmac(&key));
    }

    #[test]
    fn signed_message_fails_verification_under_a_different_key() {
        let key = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[3u8; 32]);
        let other = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[4u8; 32]);
        let signed = a_message().signed(&key).unwrap();
        assert!(!signed.verify_hmac(&other));
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

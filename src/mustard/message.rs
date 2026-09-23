/// Gossip message types.
///
/// All messages are fixed-size to keep UDP datagrams under the MTU
/// (max 1400 bytes). Membership updates are piggybacked on every
/// PING/ACK exchange, achieving O(log N) convergence without
/// dedicated broadcast messages.
use std::collections::BTreeMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::meat::NodeId;

use super::state::NodeState;

/// Maximum number of piggybacked membership updates per gossip message.
/// Bounded to keep message size constant (~512 bytes total).
pub const MAX_PIGGYBACK_UPDATES: usize = 8;

/// Most label keys a directory extension will carry. Labels ride every
/// gossip datagram, so an unbounded set would blow past the UDP budget;
/// past this many keys we drop the rest (deterministically — `BTreeMap`
/// iterates in key order, so every node truncates the same way).
pub const MAX_DIRECTORY_LABELS: usize = 16;

/// Longest label key or value we advertise. A key or value longer than
/// this is skipped rather than truncated mid-string (a truncated value
/// would silently mismatch a placement constraint).
pub const MAX_DIRECTORY_LABEL_LEN: usize = 64;

/// Total bytes of label key+value data we advertise. This is the real
/// guard: even 16 keys of 64 bytes each would overflow the MTU alongside
/// the message body, so we stop accumulating once this budget is hit.
const MAX_DIRECTORY_LABEL_BYTES: usize = 512;

/// Top-level gossip message sent as a single UDP datagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Protocol version for forward compatibility.
    pub version: u8,
    /// Durable state generation required for cluster admission.
    pub state_format: u32,
    /// Sender's node identity.
    pub sender: NodeId,
    /// Sender's current incarnation number.
    pub incarnation: u64,
    /// HMAC-SHA256 over the rest of the message. Zeroed at construction; the
    /// transport signs it on send and verifies it on receive when keyed.
    pub hmac: [u8; 32],
    /// The message payload.
    pub payload: GossipPayload,
    /// Optional directory data outside the positional bincode message body.
    /// Peers within the same protocol/state generation may omit or ignore
    /// this extension. Unsupported development generations are refused first.
    #[serde(skip)]
    pub extension: Option<DirectoryExtension>,
}

impl GossipMessage {
    /// Current protocol version.
    pub const VERSION: u8 = crate::compatibility::CURRENT.protocol as u8;

    /// Create a new gossip message with the given sender and payload.
    /// HMAC is zeroed; the transport signs it on send when a key is configured.
    pub fn new(sender: NodeId, incarnation: u64, payload: GossipPayload) -> Self {
        Self {
            version: Self::VERSION,
            state_format: crate::compatibility::CURRENT.state,
            sender,
            incarnation,
            hmac: [0u8; 32],
            payload,
            extension: None,
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
        /// Whether this ACK was *forwarded* by a relay on behalf of the
        /// `sender` (an indirect-probe rescue), rather than sent directly by
        /// the sender itself. On a relayed ACK the transport socket is the
        /// relay's, not the sender's, so the receiver must NOT record the
        /// sender's address from it (M13).
        relayed: bool,
    },
}

impl GossipPayload {
    /// Extract the piggybacked updates from any payload variant.
    pub fn updates(&self) -> &[MembershipUpdate] {
        match self {
            GossipPayload::Ping { updates }
            | GossipPayload::PingReq { updates, .. }
            | GossipPayload::Ack { updates, .. } => updates,
        }
    }
}

/// A single membership update piggybacked on gossip messages.
///
/// Carries the node's identity, its new state and the incarnation number
/// for conflict resolution. The address is included so that nodes learning about a peer through
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
}

// ---------------------------------------------------------------------------
// Directory extension (Phase 12b.2)
// ---------------------------------------------------------------------------

/// The sender's best knowledge of the current Raft leader, carried on every
/// gossip datagram so nodes outside the voter set can route to the leader.
///
/// The leader originates its own hint (from its Raft metrics and its
/// configured endpoints); every other node just relays the highest-term hint
/// it has seen. Terms only grow in Raft, so `term` resolves conflicts: a
/// deposed leader's stale hint always loses to the new leader's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderHint {
    /// The leader's gossip node id.
    pub node_id: NodeId,
    /// The Raft term this hint was observed under.
    pub term: u64,
    /// The leader's advertised HTTP API endpoint.
    pub api_address: SocketAddr,
    /// The leader's advertised reporting-tree endpoint.
    pub reporting_address: SocketAddr,
}

/// Per-datagram node-directory extension.
///
/// Carries the sending node's advertised control-plane endpoints and its
/// best leader hint. Appended after the bincode-encoded [`GossipMessage`]
/// rather than inside it, with its own HMAC, so a missing or bad extension
/// costs directory data but never the membership payload — see
/// [`encode_datagram`].
///
/// `node_id` names the node the endpoints belong to. It is always the node
/// that physically stamped the datagram — which is NOT always
/// `GossipMessage::sender` (a relayed indirect-probe ACK is sent by the
/// relay but carries the probed target as its sender).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryExtension {
    /// The node these endpoints belong to (the stamping node).
    pub node_id: NodeId,
    /// This node's advertised HTTP API endpoint.
    pub api_address: SocketAddr,
    /// This node's advertised reporting-tree endpoint.
    pub reporting_address: SocketAddr,
    /// Best known leader, relayed epidemically; highest term wins.
    pub leader: Option<LeaderHint>,
    /// The stamping node's placement labels (zone, rack, gpu_model, …).
    /// Bounded on the way out by [`bounded_labels`] so gossip datagrams
    /// stay under the UDP budget. `BTreeMap` for a deterministic wire
    /// order (a `HashMap` would break the HMAC across nodes).
    pub labels: BTreeMap<String, String>,
    /// Whether the stamping node's disk has been under sustained pressure long
    /// enough that it should resign its council seat (12b.2 T3). A voter only
    /// knows its OWN disk locally; advertising this bit is how the leader (who
    /// runs the reconciler) learns which OTHER voters are pressured and must be
    /// replaced. The extension HMAC covers it, so a flipped `disk_pressured`
    /// bit fails verification.
    pub disk_pressured: bool,
    /// HMAC-SHA256 over the carrying message's canonical bytes plus this
    /// extension with `hmac` zeroed. Zeroed when gossip runs unkeyed.
    pub hmac: [u8; 32],
}

/// Trim a label set to what a gossip datagram can safely carry: at most
/// [`MAX_DIRECTORY_LABELS`] keys, each key and value at most
/// [`MAX_DIRECTORY_LABEL_LEN`] bytes. Over-long entries are skipped, not
/// truncated (a half a value would mis-match a placement constraint).
pub fn bounded_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut bytes = 0usize;
    for (k, v) in labels {
        if out.len() >= MAX_DIRECTORY_LABELS {
            break;
        }
        if k.len() > MAX_DIRECTORY_LABEL_LEN || v.len() > MAX_DIRECTORY_LABEL_LEN {
            continue;
        }
        let cost = k.len() + v.len();
        if bytes + cost > MAX_DIRECTORY_LABEL_BYTES {
            break;
        }
        bytes += cost;
        out.insert(k.clone(), v.clone());
    }
    out
}

impl DirectoryExtension {
    /// The canonical bytes the extension HMAC covers: the extension with
    /// `hmac` zeroed. Combined with the carrying message's canonical bytes
    /// so a valid extension can't be detached and replayed onto another
    /// message.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        let mut canonical = self.clone();
        canonical.hmac = [0u8; 32];
        bincode::serialize(&canonical)
    }

    /// Return a copy with `hmac` set over `message_canonical` plus this
    /// extension's canonical bytes, under `key`.
    pub fn signed(
        mut self,
        key: &ring::hmac::Key,
        message_canonical: &[u8],
    ) -> Result<Self, bincode::Error> {
        self.hmac = [0u8; 32];
        let mut bytes = message_canonical.to_vec();
        bytes.extend(self.canonical_bytes()?);
        let tag = crate::sesame::mtls::gossip_hmac::sign(key, &bytes);
        // HMAC-SHA256 is always 32 bytes.
        self.hmac.copy_from_slice(&tag);
        Ok(self)
    }

    /// Verify this extension's `hmac` against the carrying message's
    /// canonical bytes under `key`. Returns `false` on any serialisation
    /// error or tag mismatch.
    pub fn verify_hmac(&self, key: &ring::hmac::Key, message_canonical: &[u8]) -> bool {
        match self.canonical_bytes() {
            Ok(ext_bytes) => {
                let mut bytes = message_canonical.to_vec();
                bytes.extend(ext_bytes);
                crate::sesame::mtls::gossip_hmac::verify(key, &bytes, &self.hmac)
            }
            Err(_) => false,
        }
    }
}

/// Encode a gossip message (and its directory extension, if any) into a
/// single datagram: `bincode(message) || bincode(extension)`.
///
/// The extension is optional within one explicit format generation. Bincode
/// fields are positional, so extension bytes follow the fixed message body.
pub fn encode_datagram(message: &GossipMessage) -> Result<Vec<u8>, bincode::Error> {
    // `extension` is encoded separately from the fixed message body.
    let mut bytes = bincode::serialize(message)?;
    if let Some(extension) = &message.extension {
        bytes.extend(bincode::serialize(extension)?);
    }
    Ok(bytes)
}

/// Decode a supported datagram, rejecting protocol/state mismatches.
/// An absent or malformed directory extension does not invalidate the
/// membership payload; callers still require endpoint/readiness evidence.
pub fn decode_datagram(bytes: &[u8]) -> Result<GossipMessage, bincode::Error> {
    if bytes.first() != Some(&GossipMessage::VERSION) {
        return Err(Box::new(bincode::ErrorKind::Custom(
            "incompatible gossip protocol".into(),
        )));
    }

    // The HMAC lives *inside* the encoded message, so we cannot authenticate
    // before decoding — decoding is how we get the tag. That makes this the
    // one place where unauthenticated bytes from any sender reach a
    // deserialiser, so bound what that deserialiser may allocate (O20).
    //
    // A length prefix inside a 1500-byte datagram can claim a multi-gigabyte
    // `Vec`, and unbounded bincode will faithfully try to reserve it: the same
    // "a number is a promise, not a fact" mistake as the Raft frame reader
    // (O6), one layer down. Limiting the budget to the datagram's own length
    // can't reject anything legitimate — a message that arrived in this many
    // bytes was encoded in this many bytes — and makes an over-long claim a
    // decode error instead of an allocation.
    //
    // The encoding must stay byte-identical to `bincode::serialize`, which
    // `encode_datagram` uses and every peer in this generation speaks. `bincode`'s
    // builder API defaults to *varint* encoding, so the fixed-width shape has to
    // be asked for explicitly — `with_fixint_encoding().with_little_endian()`.
    // Getting this wrong wouldn't fail to compile, it would fail to talk to
    // the rest of the cluster, so `fixed_width_wire_bytes_decode_unchanged` pins it.
    use bincode::Options;
    let mut cursor = std::io::Cursor::new(bytes);
    let mut message: GossipMessage = datagram_codec(bytes.len()).deserialize_from(&mut cursor)?;
    if message.state_format != crate::compatibility::CURRENT.state {
        return Err(Box::new(bincode::ErrorKind::Custom(
            "incompatible gossip state format".into(),
        )));
    }
    let consumed = cursor.position() as usize;
    if consumed < bytes.len()
        && let Ok(extension) =
            datagram_codec(bytes.len()).deserialize::<DirectoryExtension>(&bytes[consumed..])
    {
        message.extension = Some(extension);
    }
    Ok(message)
}

/// The datagram decoder: the fixed-width bincode wire encoding, with reads bounded
/// to `limit` bytes (O20).
fn datagram_codec(limit: usize) -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .allow_trailing_bytes()
        .with_limit(limit as u64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompatible_gossip_generation_is_refused() {
        for version in [1, 255] {
            let mut message = a_message();
            message.version = version;
            assert!(decode_datagram(&encode_datagram(&message).unwrap()).is_err());
        }
    }

    #[test]
    fn incompatible_gossip_state_format_is_refused() {
        let mut message = a_message();
        message.state_format = 99;
        assert!(decode_datagram(&encode_datagram(&message).unwrap()).is_err());
    }

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
        let payload = GossipPayload::Ack {
            updates: vec![],
            relayed: false,
        };
        assert!(payload.updates().is_empty());
    }

    #[test]
    fn membership_update_serialisation_round_trip() {
        let update = MembershipUpdate {
            node_id: NodeId::new("node-1"),
            address: test_addr(),
            state: NodeState::Suspect,
            incarnation: 42,
        };
        let json = serde_json::to_string(&update).unwrap();
        let decoded: MembershipUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, decoded);
    }

    // -- directory extension wire tolerance (12b.2) ---------------------------

    fn an_extension() -> DirectoryExtension {
        DirectoryExtension {
            node_id: NodeId::new("sender"),
            api_address: SocketAddr::from(([127, 0, 0, 1], 9117)),
            reporting_address: SocketAddr::from(([127, 0, 0, 1], 9445)),
            leader: Some(LeaderHint {
                node_id: NodeId::new("leader"),
                term: 7,
                api_address: SocketAddr::from(([10, 0, 0, 1], 9117)),
                reporting_address: SocketAddr::from(([10, 0, 0, 1], 9445)),
            }),
            labels: BTreeMap::from([("zone".to_string(), "us-east".to_string())]),
            disk_pressured: false,
            hmac: [0u8; 32],
        }
    }

    #[test]
    fn datagram_round_trip_preserves_the_extension() {
        let mut msg = a_message();
        msg.extension = Some(an_extension());
        let datagram = encode_datagram(&msg).unwrap();

        let decoded = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded.extension, Some(an_extension()));
        assert_eq!(decoded.sender, msg.sender);
    }

    #[test]
    fn garbage_trailing_bytes_drop_the_extension_not_the_message() {
        let msg = a_message();
        let mut datagram = encode_datagram(&msg).unwrap();
        datagram.extend_from_slice(&[0xFF, 0x01]);

        let decoded = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded.sender, msg.sender);
        // Two junk bytes can't decode as an extension — dropped silently.
        assert!(decoded.extension.is_none());
    }

    #[test]
    fn extension_is_excluded_from_message_canonical_bytes() {
        let mut with = a_message();
        with.extension = Some(an_extension());
        let without = a_message();
        // The message HMAC must not change when an extension is attached,
        // so a datagram whose extension is dropped still verifies.
        assert_eq!(
            with.canonical_bytes().unwrap(),
            without.canonical_bytes().unwrap()
        );
    }

    #[test]
    fn signed_extension_verifies_and_rejects_detach_and_tamper() {
        let key = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[3u8; 32]);
        let msg = a_message().signed(&key).unwrap();
        let canonical = msg.canonical_bytes().unwrap();
        let ext = an_extension().signed(&key, &canonical).unwrap();
        assert!(ext.verify_hmac(&key, &canonical));

        // Tampered content fails.
        let mut tampered = ext.clone();
        tampered.api_address = SocketAddr::from(([9, 9, 9, 9], 1));
        assert!(!tampered.verify_hmac(&key, &canonical));

        // Detached onto a different message fails.
        let other = GossipMessage::new(
            NodeId::new("other"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        assert!(!ext.verify_hmac(&key, &other.canonical_bytes().unwrap()));
    }

    #[test]
    fn extension_stays_within_the_udp_budget() {
        let mut msg = a_message();
        msg.extension = Some(an_extension());
        let plain = bincode::serialize(&msg).unwrap();
        let datagram = encode_datagram(&msg).unwrap();
        // The extension costs well under 200 bytes of the 1400-byte budget.
        assert!(
            datagram.len() - plain.len() < 200,
            "extension too large: {} bytes",
            datagram.len() - plain.len()
        );
    }

    #[test]
    fn extension_carries_labels_round_trip() {
        let mut msg = a_message();
        msg.extension = Some(an_extension());
        let datagram = encode_datagram(&msg).unwrap();
        let decoded = decode_datagram(&datagram).unwrap();
        let labels = &decoded.extension.unwrap().labels;
        assert_eq!(labels.get("zone").map(String::as_str), Some("us-east"));
    }

    #[test]
    fn extension_carries_disk_pressured_round_trip() {
        let mut ext = an_extension();
        ext.disk_pressured = true;
        let mut msg = a_message();
        msg.extension = Some(ext);
        let datagram = encode_datagram(&msg).unwrap();
        let decoded = decode_datagram(&datagram).unwrap();
        assert!(decoded.extension.unwrap().disk_pressured);
    }

    #[test]
    fn signed_extension_rejects_a_flipped_disk_pressured_bit() {
        let key = crate::sesame::mtls::gossip_hmac::derive_gossip_key(&[3u8; 32]);
        let msg = a_message().signed(&key).unwrap();
        let canonical = msg.canonical_bytes().unwrap();
        let mut ext = an_extension();
        ext.disk_pressured = false;
        let signed = ext.signed(&key, &canonical).unwrap();
        assert!(signed.verify_hmac(&key, &canonical));

        // Flip only the disk-pressure bit: the HMAC must reject it.
        let mut tampered = signed.clone();
        tampered.disk_pressured = true;
        assert!(!tampered.verify_hmac(&key, &canonical));
    }

    #[test]
    fn bounded_labels_caps_key_count_and_length() {
        let mut labels = BTreeMap::new();
        for i in 0..100 {
            labels.insert(format!("k{i:03}"), "v".to_string());
        }
        labels.insert("toolong".to_string(), "x".repeat(200));
        let bounded = bounded_labels(&labels);
        assert_eq!(bounded.len(), MAX_DIRECTORY_LABELS);
        // The over-long value was skipped entirely.
        assert!(!bounded.contains_key("toolong"));
        // Truncation is deterministic (lowest keys survive).
        assert!(bounded.contains_key("k000"));
    }

    #[test]
    fn labelled_extension_stays_within_the_udp_budget() {
        let mut labels = BTreeMap::new();
        for i in 0..MAX_DIRECTORY_LABELS {
            labels.insert(format!("key{i:02}"), "value".repeat(4));
        }
        let mut ext = an_extension();
        ext.labels = bounded_labels(&labels);
        let mut msg = a_message();
        msg.extension = Some(ext);
        let plain = bincode::serialize(&msg).unwrap();
        let datagram = encode_datagram(&msg).unwrap();
        // A full label set plus endpoints stays well under the 1400-byte MTU.
        assert!(
            datagram.len() < 1400,
            "labelled datagram too large: {} bytes",
            datagram.len()
        );
        assert!(datagram.len() > plain.len());
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
                }],
            },
        );
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: GossipMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.version, msg.version);
        assert_eq!(decoded.sender, msg.sender);
        assert_eq!(decoded.incarnation, msg.incarnation);
    }

    /// The decoder uses bincode's `Options` builder, whose default is
    /// *varint* encoding — a silent wire-format change that compiles
    /// perfectly and simply stops talking to every peer. Pin the fixed-width
    /// shape against bytes produced by `bincode::serialize` directly, which
    /// is what peers send.
    #[test]
    fn fixed_width_wire_bytes_decode_unchanged() {
        let msg = GossipMessage::new(
            NodeId::new("peer"),
            42,
            GossipPayload::Ack {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("target"),
                    address: test_addr(),
                    state: NodeState::Suspect,
                    incarnation: 7,
                }],
                relayed: false,
            },
        );
        // Exactly what a peer puts on the wire: plain `bincode::serialize`.
        let bytes = bincode::serialize(&msg).expect("encode");
        let decoded = decode_datagram(&bytes).expect("peer bytes must decode");

        assert_eq!(decoded.version, msg.version);
        assert_eq!(decoded.sender, msg.sender);
        assert_eq!(decoded.incarnation, 42);
        assert_eq!(decoded.payload.updates().len(), 1);
        assert_eq!(decoded.payload.updates()[0].incarnation, 7);
        assert!(
            decoded.extension.is_none(),
            "a bare datagram carries no directory extension"
        );
    }

    /// O20: `decode_datagram` is the one place unauthenticated bytes from any
    /// sender reach a deserialiser — the HMAC is inside the message, so we
    /// have to decode to get the tag. An unbounded bincode will honour a
    /// length prefix that claims gigabytes, from a datagram of a few dozen
    /// bytes.
    #[test]
    fn a_datagram_claiming_more_than_it_carries_is_a_decode_error() {
        // A real message, then a hostile one built by overwriting the first
        // Vec length prefix we can reach with an enormous value. Rather than
        // hand-assemble the encoding, take a valid datagram and corrupt its
        // length fields one at a time — every position that decodes must not
        // allocate beyond the datagram.
        let msg = GossipMessage::new(
            NodeId::new("sender"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("target"),
                    address: test_addr(),
                    state: NodeState::Alive,
                    incarnation: 1,
                }],
            },
        );
        let valid = encode_datagram(&msg).expect("encode");
        assert!(decode_datagram(&valid).is_ok(), "baseline must decode");

        // Any 8-byte window read as a little-endian u64 length becomes a
        // gigantic claim. None of them may succeed with a huge allocation;
        // each must either fail or decode within the datagram's budget.
        let mut rejected = 0;
        for offset in 0..valid.len().saturating_sub(8) {
            let mut hostile = valid.clone();
            hostile[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            if decode_datagram(&hostile).is_err() {
                rejected += 1;
            }
        }
        assert!(
            rejected > 0,
            "no corrupted length was rejected — the decode budget is not being applied"
        );
    }

    /// The bound must not reject anything a peer can legitimately send, so a
    /// full-size datagram (many updates, labels, endpoints) still decodes.
    #[test]
    fn a_large_but_legitimate_datagram_still_decodes() {
        let updates: Vec<MembershipUpdate> = (0..MAX_PIGGYBACK_UPDATES)
            .map(|i| MembershipUpdate {
                node_id: NodeId::new(format!("node-with-a-fairly-long-name-{i}")),
                address: test_addr(),
                state: NodeState::Suspect,
                incarnation: u64::MAX,
            })
            .collect();
        let msg = GossipMessage::new(
            NodeId::new("sender"),
            u64::MAX,
            GossipPayload::Ping { updates },
        );
        let bytes = encode_datagram(&msg).expect("encode");
        let decoded = decode_datagram(&bytes).expect("a full-size datagram must decode");
        assert_eq!(decoded.sender, msg.sender);
    }
}

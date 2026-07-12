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
    /// Sender's node identity.
    pub sender: NodeId,
    /// Sender's current incarnation number.
    pub incarnation: u64,
    /// HMAC-SHA256 over the rest of the message. Zeroed at construction; the
    /// transport signs it on send and verifies it on receive when keyed.
    pub hmac: [u8; 32],
    /// The message payload.
    pub payload: GossipPayload,
    /// Node-directory extension (Phase 12b.2). `#[serde(skip)]` keeps it out
    /// of the bincode message body — bincode is positional, so a new field
    /// inside the message would break every older decoder. The UDP transport
    /// instead appends the encoded extension AFTER the message bytes (see
    /// [`encode_datagram`]/[`decode_datagram`]); older peers ignore the
    /// trailing bytes and newer peers pick them up. In-memory transports
    /// clone the whole struct, so the extension flows through untouched.
    #[serde(skip)]
    pub extension: Option<DirectoryExtension>,
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
/// rather than inside it, so older peers (which stop reading at the end of
/// the message) still parse the datagram — see [`encode_datagram`].
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
    ///
    /// `#[serde(default)]` is deliberately NOT used here: the extension
    /// rides as trailing bytes (see [`encode_datagram`]), so the whole
    /// struct is versioned by its trailing position, not per-field. A
    /// pre-labels peer sends an extension one field shorter, which a
    /// newer decoder rejects and drops (the endpoints/leader hint still
    /// come through the message body's own relayed copies). New peers
    /// always emit the field, so a mixed cluster converges on labels as
    /// the old nodes roll.
    pub labels: BTreeMap<String, String>,
    /// Whether the stamping node's disk has been under sustained pressure long
    /// enough that it should resign its council seat (12b.2 T3). A voter only
    /// knows its OWN disk locally; advertising this bit is how the leader (who
    /// runs the reconciler) learns which OTHER voters are pressured and must be
    /// replaced. It rides the same trailing, position-versioned extension as
    /// `labels`: a pre-field peer sends a shorter extension a newer decoder
    /// drops, and new peers always emit the field, so a mixed cluster converges
    /// as the old nodes roll. It sits before `hmac` for the same reason `labels`
    /// does — the extension HMAC covers everything up to (and excluding) the
    /// zeroed `hmac`, so a flipped `disk_pressured` bit fails verification.
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
/// The extension rides as trailing bytes because bincode is not
/// self-describing: `#[serde(default)]` on a new struct field gives no
/// tolerance at all (an old decoder misparses the extra bytes, a new decoder
/// hits EOF on an old message). Trailing bytes give tolerance in BOTH
/// directions — bincode's legacy `deserialize` ignores what it doesn't read,
/// so an old peer parses a new datagram, and a new peer treats a datagram
/// with no trailing bytes as extension-free.
pub fn encode_datagram(message: &GossipMessage) -> Result<Vec<u8>, bincode::Error> {
    // `extension` is #[serde(skip)], so this is exactly the old wire shape.
    let mut bytes = bincode::serialize(message)?;
    if let Some(extension) = &message.extension {
        bytes.extend(bincode::serialize(extension)?);
    }
    Ok(bytes)
}

/// Decode a datagram produced by [`encode_datagram`] (or by an older peer,
/// in which case the extension is `None`). A malformed extension is dropped
/// silently rather than failing the whole message: the membership payload is
/// still good, and gossip must keep flowing across versions.
pub fn decode_datagram(bytes: &[u8]) -> Result<GossipMessage, bincode::Error> {
    let mut cursor = std::io::Cursor::new(bytes);
    let mut message: GossipMessage = bincode::deserialize_from(&mut cursor)?;
    let consumed = cursor.position() as usize;
    if consumed < bytes.len()
        && let Ok(extension) = bincode::deserialize::<DirectoryExtension>(&bytes[consumed..])
    {
        message.extension = Some(extension);
    }
    Ok(message)
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

    /// The exact wire shape a pre-12b.2 peer serialises and parses.
    /// Field-for-field the old `GossipMessage` (no extension).
    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyGossipMessage {
        version: u8,
        sender: NodeId,
        incarnation: u64,
        hmac: [u8; 32],
        payload: GossipPayload,
    }

    #[test]
    fn old_peer_parses_a_datagram_carrying_an_extension() {
        let mut msg = a_message();
        msg.extension = Some(an_extension());
        let datagram = encode_datagram(&msg).unwrap();

        // An old peer deserialises the legacy shape from the full datagram;
        // bincode's legacy `deserialize` must ignore the trailing extension.
        let legacy: LegacyGossipMessage = bincode::deserialize(&datagram).unwrap();
        assert_eq!(legacy.sender, NodeId::new("sender"));
        assert_eq!(legacy.payload.updates().len(), 1);
    }

    #[test]
    fn new_peer_parses_an_old_datagram_as_extension_free() {
        // An old peer's datagram is just the bare message bytes.
        let legacy = LegacyGossipMessage {
            version: GossipMessage::VERSION,
            sender: NodeId::new("old-node"),
            incarnation: 3,
            hmac: [0u8; 32],
            payload: GossipPayload::Ping { updates: vec![] },
        };
        let datagram = bincode::serialize(&legacy).unwrap();

        let decoded = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded.sender, NodeId::new("old-node"));
        assert!(decoded.extension.is_none());
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
        // or old peers could no longer verify new datagrams.
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

    /// The exact wire shape a pre-labels (post-12b.2-directory) peer
    /// serialises for the extension: no `labels` field.
    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyDirectoryExtension {
        node_id: NodeId,
        api_address: SocketAddr,
        reporting_address: SocketAddr,
        leader: Option<LeaderHint>,
        hmac: [u8; 32],
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
    fn new_peer_drops_a_pre_labels_extension_but_keeps_the_message() {
        // A pre-labels peer's datagram is the message body followed by the
        // shorter extension. bincode is positional, so the trailing bytes
        // don't decode as the current `DirectoryExtension` — the extension
        // is dropped, the message body still parses (both-direction
        // tolerance, exactly as for a pre-directory peer).
        let msg = a_message();
        let legacy_ext = LegacyDirectoryExtension {
            node_id: NodeId::new("old"),
            api_address: SocketAddr::from(([127, 0, 0, 1], 9117)),
            reporting_address: SocketAddr::from(([127, 0, 0, 1], 9445)),
            leader: None,
            hmac: [0u8; 32],
        };
        let mut datagram = bincode::serialize(&msg).unwrap();
        datagram.extend(bincode::serialize(&legacy_ext).unwrap());

        let decoded = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded.sender, msg.sender);
        // The shorter legacy extension hits EOF partway through the labels
        // map; the modern decoder rejects it rather than mis-reading.
        assert!(decoded.extension.is_none());
    }

    /// The exact wire shape a pre-`disk_pressured` (but post-labels) peer
    /// serialises for the extension: labels, but no `disk_pressured` field.
    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyLabelledDirectoryExtension {
        node_id: NodeId,
        api_address: SocketAddr,
        reporting_address: SocketAddr,
        leader: Option<LeaderHint>,
        labels: BTreeMap<String, String>,
        hmac: [u8; 32],
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
    fn new_peer_drops_a_pre_disk_pressured_extension_but_keeps_the_message() {
        // A pre-`disk_pressured` peer's extension is one bool shorter. bincode
        // is positional, so the modern decoder hits EOF where it expects the
        // bool and drops the extension; the message body still parses. This is
        // the same both-direction tolerance labels already relied on.
        let msg = a_message();
        let legacy_ext = LegacyLabelledDirectoryExtension {
            node_id: NodeId::new("old"),
            api_address: SocketAddr::from(([127, 0, 0, 1], 9117)),
            reporting_address: SocketAddr::from(([127, 0, 0, 1], 9445)),
            leader: None,
            labels: BTreeMap::from([("zone".to_string(), "us-east".to_string())]),
            hmac: [0u8; 32],
        };
        let mut datagram = bincode::serialize(&msg).unwrap();
        datagram.extend(bincode::serialize(&legacy_ext).unwrap());

        let decoded = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded.sender, msg.sender);
        assert!(decoded.extension.is_none());
    }

    #[test]
    fn old_labelled_peer_parses_a_datagram_carrying_disk_pressured() {
        // The reverse direction: a new peer's datagram (with `disk_pressured`)
        // must still parse as the shorter pre-`disk_pressured` extension shape,
        // ignoring the trailing bool. bincode's legacy `deserialize` ignores
        // what it doesn't read, so labels come through and the extra byte is
        // dropped.
        let mut ext = an_extension();
        ext.disk_pressured = true;
        let mut msg = a_message();
        msg.extension = Some(ext);
        let datagram = encode_datagram(&msg).unwrap();

        let body = bincode::serialize(&msg).unwrap();
        let legacy: LegacyLabelledDirectoryExtension =
            bincode::deserialize(&datagram[body.len()..]).unwrap();
        assert_eq!(
            legacy.labels.get("zone").map(String::as_str),
            Some("us-east")
        );
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

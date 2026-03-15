/// Mustard gossip protocol.
///
/// SWIM-based cluster membership and failure detection. Every node
/// participates in the gossip mesh, probing random peers each protocol
/// period and piggybacking membership updates on PING/ACK exchanges.
///
/// This module is built incrementally across Phase 2:
/// - Step 2: state machine, membership table, messages, dissemination
/// - Step 3: transport trait, SWIM probe cycle protocol
pub mod config;
pub mod dissemination;
pub mod membership;
pub mod message;
pub mod protocol;
pub mod state;
pub mod transport;

pub use config::GossipConfig;
pub use dissemination::DisseminationQueue;
pub use membership::{MembershipSnapshot, MembershipTable, NodeMembership, ResourceSummary};
pub use message::{GossipMessage, GossipPayload, MAX_PIGGYBACK_UPDATES, MembershipUpdate};
pub use protocol::MustardNode;
pub use state::NodeState;
pub use transport::{InMemoryNetwork, InMemoryTransport, MustardTransport, UdpMustardTransport};

/// Errors from Mustard gossip operations.
#[derive(Debug, thiserror::Error)]
pub enum MustardError {
    #[error("transport send failed: {reason}")]
    SendFailed { reason: String },

    #[error("transport receive failed: {reason}")]
    RecvFailed { reason: String },

    #[error("message serialisation failed: {0}")]
    Serialisation(String),
}

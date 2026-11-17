//! The council membership check on the Raft port.
//!
//! mTLS on the Raft listener proves a peer holds a valid node certificate,
//! and every node in the cluster holds one, workers included. That alone
//! would let any node drive Raft: append entries as a made-up leader, collect
//! votes, or claim a newer recovery epoch. This module answers the question
//! the handshake can't: is the sender of this RPC someone this node's council
//! lets drive Raft?
//!
//! # Who sends what
//!
//! openraft 0.9 has three RPCs, and each has one kind of sender:
//!
//! | RPC               | Sent by    | Received by          |
//! |-------------------|------------|----------------------|
//! | `AppendEntries`   | the leader | voters and learners  |
//! | `InstallSnapshot` | the leader | voters and learners  |
//! | `Vote`            | a candidate| voters (and learners)|
//!
//! A leader and a candidate are always voters: openraft never lets a learner
//! campaign or lead. So the receiver checks the *sender*, and the rule is the
//! same for all three: the node the RPC names as its leader or candidate (its
//! vote's `voted_for`) must be a voter in this node's council, and it must be
//! the node whose certificate the connection presented. The second half stops
//! a node speaking for another one. A learner receives replication but never
//! sends an RPC, so no learner is ever admitted as a sender.
//!
//! The direction matters. A leader *dials* a learner to replicate to it, so
//! the learner is the receiver, and the check runs against the leader, which
//! is a voter. Adding a learner needs nothing from this check.
//!
//! # Which council
//!
//! "This node's council" is the union of the voters in its *committed* and
//! its *effective* membership, both from openraft's own state, never from
//! gossip. openraft makes a membership entry effective as soon as it is
//! appended, before it commits. A leader that removes itself keeps sending
//! until that entry commits, and its followers have already appended the
//! entry that drops it, so the committed membership is what still lets them
//! hear it. A voter that joins in a joint config is in the effective one.
//!
//! # A node with no council
//!
//! A fresh node, one that has never been initialised and never received a
//! membership, has nothing to check against, and the first leader to
//! replicate to it is how it joins. It serves any node certificate, as it did
//! before this check. It holds no Raft state worth protecting yet.
//!
//! # A node whose view is behind
//!
//! A member that was down or slow while the council changed can hold a
//! membership that predates the current leader: the leader joined while it
//! was away. Its own view says the leader is a stranger, and refusing it
//! forever would lock a legitimate member out. So when a stranger sends it
//! replication, it asks the voters it *does* know which nodes are voters in
//! their councils ([`super::network::RaftRpc::CouncilVoters`]), over the same
//! node-id-bound mTLS the transport always dials with. If one of them names
//! the stranger, with the same node name the certificate carries, it admits
//! that node for [`VOUCH_TTL`], long enough to catch up and learn the
//! membership from the log itself. A single voucher is enough: a voter in
//! this node's own view can already drive its Raft directly, so its word adds
//! no power it didn't have.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use openraft::{Membership, Raft, RaftNetworkFactory};
use rustls::pki_types::CertificateDer;
use tokio::time::Instant;

use super::network::{RaftRpc, TcpRaftNetworkFactory};
use super::types::{CouncilNodeInfo, TypeConfig};

/// How long a vouched-for sender stays admitted.
///
/// Long enough for a node to catch up and read the membership that names the
/// sender from its own log; short enough that a node removed from the
/// council stops being admitted soon after.
pub const VOUCH_TTL: Duration = Duration::from_secs(60);

/// The shortest gap between two rounds of asking the council's voters. A
/// leader retries replication many times a second; one round answers them all.
pub const VOUCH_INTERVAL: Duration = Duration::from_secs(2);

/// How long one voter has to answer a vouching question.
pub const VOUCH_TIMEOUT: Duration = Duration::from_secs(2);

/// The council's voters as one node sees them: Raft id to node name.
pub type CouncilVoters = BTreeMap<u64, String>;

/// Who is on the other end of a Raft connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIdentity {
    /// A plain TCP connection. Nothing is authenticated, so the RPC's own
    /// claim names the sender (development clusters without mTLS).
    Plaintext,
    /// An mTLS connection whose certificate names this node.
    Node(String),
    /// An mTLS connection whose certificate names no node. Never a member.
    Unnamed,
}

impl PeerIdentity {
    /// The identity a TLS connection's peer certificates prove.
    pub fn from_certificates(chain: Option<&[CertificateDer<'_>]>) -> Self {
        match chain
            .and_then(|chain| chain.first())
            .and_then(crate::sesame::mtls::node_id_from_leaf)
        {
            Some(name) => Self::Node(name),
            None => Self::Unnamed,
        }
    }

    /// Whether this peer may speak for the council node called `name`.
    fn speaks_for(&self, name: &str) -> bool {
        match self {
            Self::Plaintext => true,
            Self::Node(own) => own == name,
            Self::Unnamed => false,
        }
    }
}

/// Where the sender of one Raft RPC stands with this node's council.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// This node has no council yet: it serves whoever replicates to it.
    NoCouncil,
    /// The sender is a voter in this node's council (or vouched for by one),
    /// and the connection's certificate is that voter's.
    Member,
    /// Anyone else.
    Stranger,
}

/// Where `sender` stands, given this node's council `voters` (`None` when it
/// has no council), the senders its voters have vouched for, and who the
/// connection's certificate says is talking.
pub fn standing(
    voters: Option<&CouncilVoters>,
    vouched: &CouncilVoters,
    sender: Option<u64>,
    peer: &PeerIdentity,
) -> Standing {
    let Some(voters) = voters else {
        return Standing::NoCouncil;
    };
    let Some(sender) = sender else {
        return Standing::Stranger;
    };
    let known = voters.get(&sender).or_else(|| vouched.get(&sender));
    match known {
        Some(name) if peer.speaks_for(name) => Standing::Member,
        _ => Standing::Stranger,
    }
}

/// The node name of voter `id` in `membership`, if it is a voter there.
fn voter_name(membership: &Membership<u64, CouncilNodeInfo>, id: u64) -> Option<String> {
    if !membership.voter_ids().any(|voter| voter == id) {
        return None;
    }
    membership.get_node(&id).map(|node| node.name.clone())
}

/// Add `membership`'s voters, with their node names, to `voters`.
fn add_voters(voters: &mut CouncilVoters, membership: &Membership<u64, CouncilNodeInfo>) {
    for id in membership.voter_ids() {
        if let Some(node) = membership.get_node(&id) {
            voters.insert(id, node.name.clone());
        }
    }
}

/// This node's council from openraft's own state: the voters of its committed
/// and effective memberships together, or `None` when it has neither (a fresh
/// node). `None` too when the Raft core has stopped.
pub async fn council_voters(raft: &Raft<TypeConfig>) -> Option<CouncilVoters> {
    raft.with_raft_state(|state| {
        let committed = state.membership_state.committed().membership();
        let effective = state.membership_state.effective().membership();
        if committed.nodes().next().is_none() && effective.nodes().next().is_none() {
            return None;
        }
        let mut voters = CouncilVoters::new();
        add_voters(&mut voters, committed);
        add_voters(&mut voters, effective);
        Some(voters)
    })
    .await
    .ok()
    .flatten()
}

/// The voters this node could ask to vouch for a sender: every voter in its
/// council except itself and the sender, with their Raft addresses.
async fn vouchers(raft: &Raft<TypeConfig>, sender: u64) -> Vec<(u64, CouncilNodeInfo)> {
    let me = raft.metrics().borrow().id;
    raft.with_raft_state(move |state| {
        let mut found: BTreeMap<u64, CouncilNodeInfo> = BTreeMap::new();
        for membership in [
            state.membership_state.committed().membership(),
            state.membership_state.effective().membership(),
        ] {
            for id in membership.voter_ids() {
                if id == me || id == sender {
                    continue;
                }
                if let Some(node) = membership.get_node(&id) {
                    found.insert(id, node.clone());
                }
            }
        }
        found.into_iter().collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default()
}

/// A sender a voter vouched for, and until when that counts.
#[derive(Debug, Clone)]
struct Vouched {
    name: String,
    until: Instant,
}

#[derive(Debug, Default)]
struct AdmissionState {
    vouched: tokio::sync::Mutex<HashMap<u64, Vouched>>,
    last_round: tokio::sync::Mutex<Option<Instant>>,
}

/// The Raft server's membership check: the pure [`standing`] rule, fed from
/// openraft's state, plus the vouching that keeps a member whose view is
/// behind from being locked out. Cheap to clone; clones share their state.
#[derive(Clone)]
pub struct RaftAdmission {
    /// Dials the council's voters to ask them to vouch. `None` never asks.
    dialer: Option<TcpRaftNetworkFactory>,
    state: Arc<AdmissionState>,
}

impl std::fmt::Debug for RaftAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaftAdmission")
            .field("vouching", &self.dialer.is_some())
            .finish()
    }
}

impl RaftAdmission {
    /// A check that asks the council's voters, through `dialer`, about a
    /// stranger that replicates to this node. Pass the node's own Raft
    /// network factory: it dials with the node's identity, bound to the
    /// voter it means to reach.
    pub fn new(dialer: TcpRaftNetworkFactory) -> Self {
        Self {
            dialer: Some(dialer),
            state: Arc::default(),
        }
    }

    /// A check that never asks anyone: a stranger stays a stranger until
    /// this node's own log names it.
    pub fn without_vouching() -> Self {
        Self {
            dialer: None,
            state: Arc::default(),
        }
    }

    /// Where `sender` stands with this node's council, for an RPC that
    /// arrived from `peer`.
    pub async fn standing(
        &self,
        raft: &Raft<TypeConfig>,
        sender: Option<u64>,
        peer: &PeerIdentity,
    ) -> Standing {
        // Fast path: the metrics channel's effective membership. Copy the
        // name out in this one statement, so the channel's read lock is
        // released before anything else is borrowed or awaited.
        let effective_name = sender
            .and_then(|id| voter_name(raft.metrics().borrow().membership_config.membership(), id));
        if effective_name.is_some_and(|name| peer.speaks_for(&name)) {
            return Standing::Member;
        }
        // Otherwise ask the Raft core: it holds the committed membership as
        // well, and it is never behind the metrics.
        let voters = council_voters(raft).await;
        let vouched = self.vouched().await;
        standing(voters.as_ref(), &vouched, sender, peer)
    }

    /// The senders vouched for and still current.
    async fn vouched(&self) -> CouncilVoters {
        let now = Instant::now();
        let mut vouched = self.state.vouched.lock().await;
        vouched.retain(|_, entry| entry.until > now);
        vouched
            .iter()
            .map(|(id, entry)| (*id, entry.name.clone()))
            .collect()
    }

    /// Ask this node's council voters, in the background, whether `sender`
    /// (connected as `peer`) is a voter in their councils. At most one round
    /// runs per [`VOUCH_INTERVAL`].
    pub fn ask_for_vouch(&self, raft: &Raft<TypeConfig>, sender: u64, peer: &PeerIdentity) {
        let Some(dialer) = self.dialer.clone() else {
            return;
        };
        if *peer == PeerIdentity::Unnamed {
            return;
        }
        let this = self.clone();
        let raft = raft.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            this.vouch_round(dialer, raft, sender, peer).await;
        });
    }

    async fn vouch_round(
        &self,
        dialer: TcpRaftNetworkFactory,
        raft: Raft<TypeConfig>,
        sender: u64,
        peer: PeerIdentity,
    ) {
        {
            let mut last = self.state.last_round.lock().await;
            if last.is_some_and(|at| at.elapsed() < VOUCH_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
        }
        let vouchers = vouchers(&raft, sender).await;
        let asked: Vec<u64> = vouchers.iter().map(|(id, _)| *id).collect();
        let answers = futures_util::future::join_all(vouchers.into_iter().map(|(id, info)| {
            let mut dialer = dialer.clone();
            async move {
                let network = dialer.new_client(id, &info).await;
                let answer = tokio::time::timeout(VOUCH_TIMEOUT, network.council_voters()).await;
                (id, answer.ok().and_then(Result::ok))
            }
        }))
        .await;
        let voucher = answers.into_iter().find_map(|(id, voters)| {
            let name = voters?.get(&sender)?.clone();
            peer.speaks_for(&name).then_some((id, name))
        });
        let Some((voucher, name)) = voucher else {
            eprintln!(
                "raft: refused replication from node {sender} ({peer:?}): it isn't a voter in \
                 this node's council, and none of the voters asked ({asked:?}) names it"
            );
            return;
        };
        eprintln!(
            "raft: council voter {voucher} names node {sender} ({name}) as a voter; admitting \
             it for {}s while this node catches up",
            VOUCH_TTL.as_secs()
        );
        self.state.vouched.lock().await.insert(
            sender,
            Vouched {
                name,
                until: Instant::now() + VOUCH_TTL,
            },
        );
    }
}

impl RaftRpc {
    /// The node this RPC speaks for: the leader that sends replication, or
    /// the candidate that asks for a vote. `None` for a question that speaks
    /// for nobody.
    pub fn sender(&self) -> Option<u64> {
        let vote = match self {
            RaftRpc::AppendEntries(request) => &request.vote,
            RaftRpc::Vote(request) => &request.vote,
            RaftRpc::InstallSnapshot(request) => &request.vote,
            RaftRpc::CouncilVoters => return None,
        };
        vote.leader_id().voted_for()
    }

    /// Whether this RPC replicates the log (only a leader sends these).
    pub fn replicates(&self) -> bool {
        matches!(
            self,
            RaftRpc::AppendEntries(_) | RaftRpc::InstallSnapshot(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voters(entries: &[(u64, &str)]) -> CouncilVoters {
        entries
            .iter()
            .map(|(id, name)| (*id, (*name).to_string()))
            .collect()
    }

    fn node(name: &str) -> PeerIdentity {
        PeerIdentity::Node(name.to_string())
    }

    #[test]
    fn a_node_with_no_council_serves_anyone() {
        let none = CouncilVoters::new();
        assert_eq!(
            standing(None, &none, Some(9), &node("n9")),
            Standing::NoCouncil
        );
    }

    #[test]
    fn a_voter_presenting_its_own_certificate_is_a_member() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, Some(2), &node("n2")),
            Standing::Member
        );
    }

    #[test]
    fn a_node_outside_the_council_is_a_stranger() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, Some(9), &node("n9")),
            Standing::Stranger
        );
    }

    #[test]
    fn a_certificate_cannot_speak_for_another_voter() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, Some(2), &node("n9")),
            Standing::Stranger
        );
        assert_eq!(
            standing(Some(&council), &none, Some(2), &PeerIdentity::Unnamed),
            Standing::Stranger
        );
    }

    #[test]
    fn an_rpc_that_names_no_sender_is_a_strangers() {
        let council = voters(&[(1, "n1")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, None, &node("n1")),
            Standing::Stranger
        );
    }

    #[test]
    fn plaintext_trusts_the_rpcs_claim_but_still_checks_the_council() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, Some(2), &PeerIdentity::Plaintext),
            Standing::Member
        );
        assert_eq!(
            standing(Some(&council), &none, Some(9), &PeerIdentity::Plaintext),
            Standing::Stranger
        );
    }

    #[test]
    fn a_vouched_sender_is_a_member_under_the_vouched_name_only() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let vouched = voters(&[(3, "n3")]);
        assert_eq!(
            standing(Some(&council), &vouched, Some(3), &node("n3")),
            Standing::Member
        );
        assert_eq!(
            standing(Some(&council), &vouched, Some(3), &node("n9")),
            Standing::Stranger
        );
    }

    #[test]
    fn only_a_leader_or_candidate_rpc_names_a_sender() {
        use openraft::raft::{AppendEntriesRequest, VoteRequest};
        let append = RaftRpc::AppendEntries(AppendEntriesRequest {
            vote: openraft::Vote::new_committed(3, 7),
            prev_log_id: None,
            entries: Vec::new(),
            leader_commit: None,
        });
        assert_eq!(append.sender(), Some(7));
        assert!(append.replicates());
        let vote = RaftRpc::Vote(VoteRequest::new(openraft::Vote::new(3, 5), None));
        assert_eq!(vote.sender(), Some(5));
        assert!(!vote.replicates());
        assert_eq!(RaftRpc::CouncilVoters.sender(), None);
        assert!(!RaftRpc::CouncilVoters.replicates());
    }
}

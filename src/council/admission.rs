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
//! membership, has no council of its own to check against, and the first
//! leader to replicate to it is how it joins. It used to serve any node
//! certificate, on the reasoning that it held no Raft state worth
//! protecting. That was wrong (#2397): whatever it was fed became its
//! replicated state, API tokens, CRL and trust set included, and nodes past
//! the council cap stay seatless for their whole lives. One compromised
//! worker could take them all over.
//!
//! So a node with no council checks the sender against the council it
//! *expects* ([`ExpectedCouncil`]): the voters the leader named in the join
//! bundle when the node enrolled, saved beside its identity and kept current
//! by [`RaftAdmission::maintain_expected_council`]. The sender must be one of
//! those voters, or one they vouch for, on a connection whose certificate is
//! that voter's, exactly as for a member. A plaintext cluster has no
//! certificates and no join, so a node there with no expected council still
//! serves whoever replicates to it: nothing on such a cluster is
//! authenticated.
//!
//! # A node whose view is behind
//!
//! A member that was down or slow while the council changed can hold a
//! membership that predates the current leader: the leader joined while it
//! was away. Its own view says the leader is a stranger, and refusing it
//! forever would lock a legitimate member out. Its vote matters too: with
//! the old leader gone, a voter added while it lagged may need its vote to
//! win. So when a stranger sends it replication, or asks for its vote with
//! a log at least as long as its own, it asks the voters it *does* know which nodes are voters in
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

/// The council's voters with their Raft addresses: Raft id to node name and
/// address. What a voter answers a [`RaftRpc::CouncilVoters`] question with,
/// and what a node with no council expects.
pub type CouncilRoster = BTreeMap<u64, CouncilNodeInfo>;

/// The file, in a node's identity directory, holding its expected council.
pub const EXPECTED_COUNCIL_FILE: &str = "council.json";

/// The file, in a node's Raft directory, naming the voters of the council a
/// recovery replaced (#2406). Node-local: it sits beside the fence record and
/// never replicates.
pub const PREVIOUS_COUNCIL_FILE: &str = "previous-council.json";

/// The voters of the councils this node's recoveries replaced, as
/// `relish council recover` saved them in `raft_dir`. Empty when it was
/// never recovered.
pub fn load_previous_council(raft_dir: &std::path::Path) -> std::io::Result<CouncilRoster> {
    match std::fs::read(raft_dir.join(PREVIOUS_COUNCIL_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CouncilRoster::new()),
        Err(error) => Err(error),
    }
}

/// Save `roster` as the previous council in `raft_dir`, atomically.
pub fn save_previous_council(
    raft_dir: &std::path::Path,
    roster: &CouncilRoster,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(roster).map_err(std::io::Error::other)?;
    crate::sesame::identity::atomic_write_mode(
        &raft_dir.join(PREVIOUS_COUNCIL_FILE),
        &bytes,
        Some(0o644),
    )
}

/// How often a node refreshes its expected council: from its own membership
/// when it has one, otherwise by asking the voters it expects.
pub const EXPECTED_COUNCIL_REFRESH: Duration = Duration::from_secs(30);

/// The names in `roster`.
pub fn roster_names(roster: &CouncilRoster) -> CouncilVoters {
    roster
        .iter()
        .map(|(id, info)| (*id, info.name.clone()))
        .collect()
}

/// The council a node with no council of its own expects to hear from
/// (#2397): the voters the leader named when the node joined, kept current
/// since. Cheap to clone; clones share the roster.
#[derive(Debug, Clone, Default)]
pub struct ExpectedCouncil {
    roster: Arc<tokio::sync::RwLock<CouncilRoster>>,
    file: Option<std::path::PathBuf>,
}

impl ExpectedCouncil {
    /// An in-memory expected council holding `roster`.
    pub fn new(roster: CouncilRoster) -> Self {
        Self {
            roster: Arc::new(tokio::sync::RwLock::new(roster)),
            file: None,
        }
    }

    /// The expected council saved in `identity_dir`, written back there as it
    /// changes. Empty when nothing is saved yet.
    pub fn load(identity_dir: &std::path::Path) -> std::io::Result<Self> {
        let file = identity_dir.join(EXPECTED_COUNCIL_FILE);
        let roster = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CouncilRoster::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            roster: Arc::new(tokio::sync::RwLock::new(roster)),
            file: Some(file),
        })
    }

    /// Save `roster` as the expected council in `identity_dir`, atomically.
    /// `relish join` calls this with the voters the join bundle names.
    pub fn save(identity_dir: &std::path::Path, roster: &CouncilRoster) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(roster).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(identity_dir)?;
        crate::sesame::identity::atomic_write_mode(
            &identity_dir.join(EXPECTED_COUNCIL_FILE),
            &bytes,
            Some(0o644),
        )
    }

    /// The voters this node expects.
    pub async fn roster(&self) -> CouncilRoster {
        self.roster.read().await.clone()
    }

    /// Replace the expected council with `roster`, and save it when it
    /// changed. An empty roster is ignored: it would lock the node out.
    pub async fn replace(&self, roster: CouncilRoster) -> std::io::Result<()> {
        if roster.is_empty() {
            return Ok(());
        }
        {
            let mut current = self.roster.write().await;
            if *current == roster {
                return Ok(());
            }
            *current = roster.clone();
        }
        let Some(file) = self.file.clone() else {
            return Ok(());
        };
        let Some(directory) = file.parent().map(std::path::Path::to_path_buf) else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || Self::save(&directory, &roster))
            .await
            .map_err(std::io::Error::other)?
    }
}

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
    /// This node has no council yet, and the sender is a voter of the
    /// council it expects (or vouched for by one) speaking with its own
    /// certificate. On a plaintext cluster with no expected council, anyone.
    NoCouncil,
    /// The sender is a voter in this node's council (or vouched for by one),
    /// and the connection's certificate is that voter's.
    Member,
    /// Anyone else.
    Stranger,
}

/// Where `sender` stands, given this node's council `voters` (`None` when it
/// has no council), the council it `expected`s while it has none, the
/// senders its voters have vouched for, and who the connection's certificate
/// says is talking.
pub fn standing(
    voters: Option<&CouncilVoters>,
    expected: &CouncilVoters,
    vouched: &CouncilVoters,
    sender: Option<u64>,
    peer: &PeerIdentity,
) -> Standing {
    let (council, admitted) = match voters {
        Some(voters) => (voters, Standing::Member),
        // Nothing to authenticate against on a plaintext cluster that never
        // joined through `relish join`.
        None if expected.is_empty() && *peer == PeerIdentity::Plaintext => {
            return Standing::NoCouncil;
        }
        None => (expected, Standing::NoCouncil),
    };
    let Some(sender) = sender else {
        return Standing::Stranger;
    };
    let known = council.get(&sender).or_else(|| vouched.get(&sender));
    match known {
        Some(name) if peer.speaks_for(name) => admitted,
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

/// Add `membership`'s voters, with their node names and addresses, to
/// `roster`.
fn add_voters(roster: &mut CouncilRoster, membership: &Membership<u64, CouncilNodeInfo>) {
    for id in membership.voter_ids() {
        if let Some(node) = membership.get_node(&id) {
            roster.insert(id, node.clone());
        }
    }
}

/// This node's council from openraft's own state: the voters of its committed
/// and effective memberships together, with their addresses, or `None` when
/// it has neither (a fresh node). `None` too when the Raft core has stopped.
pub async fn council_roster(raft: &Raft<TypeConfig>) -> Option<CouncilRoster> {
    raft.with_raft_state(|state| {
        let committed = state.membership_state.committed().membership();
        let effective = state.membership_state.effective().membership();
        if committed.nodes().next().is_none() && effective.nodes().next().is_none() {
            return None;
        }
        let mut roster = CouncilRoster::new();
        add_voters(&mut roster, committed);
        add_voters(&mut roster, effective);
        Some(roster)
    })
    .await
    .ok()
    .flatten()
}

/// [`council_roster`], names only.
pub async fn council_voters(raft: &Raft<TypeConfig>) -> Option<CouncilVoters> {
    council_roster(raft)
        .await
        .map(|roster| roster_names(&roster))
}

/// The voters this node could ask: every voter in its council, or in the
/// council it expects while it has none, except itself and `skip`, with
/// their Raft addresses.
async fn askable_voters(
    raft: &Raft<TypeConfig>,
    expected: &ExpectedCouncil,
    skip: Option<u64>,
) -> Vec<(u64, CouncilNodeInfo)> {
    let me = raft.metrics().borrow().id;
    let roster = match council_roster(raft).await {
        Some(roster) => roster,
        None => expected.roster().await,
    };
    roster
        .into_iter()
        .filter(|(id, _)| *id != me && Some(*id) != skip)
        .collect()
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
    /// The council this node expects while it has none of its own.
    expected: ExpectedCouncil,
    /// The voters of the councils this node's recoveries replaced.
    previous: Arc<CouncilRoster>,
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
            expected: ExpectedCouncil::default(),
            previous: Arc::default(),
            state: Arc::default(),
        }
    }

    /// A check that never asks anyone: a stranger stays a stranger until
    /// this node's own log names it.
    pub fn without_vouching() -> Self {
        Self {
            dialer: None,
            expected: ExpectedCouncil::default(),
            previous: Arc::default(),
            state: Arc::default(),
        }
    }

    /// The same check, expecting `expected` while this node has no council.
    pub fn expecting(mut self, expected: ExpectedCouncil) -> Self {
        self.expected = expected;
        self
    }

    /// The same check, also asking `previous`, the voters of the councils
    /// this node's recoveries replaced, about an epoch claim on gossip
    /// (#2406). A recovered council starts with one voter, so without them
    /// two survivors recovered on one epoch would have nobody to ask, and
    /// would never see the split (#694).
    pub fn remembering(mut self, previous: CouncilRoster) -> Self {
        self.previous = Arc::new(previous);
        self
    }

    /// The council this check expects while the node has none.
    pub fn expected_council(&self) -> &ExpectedCouncil {
        &self.expected
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
        let expected = roster_names(&self.expected.roster().await);
        let vouched = self.vouched().await;
        standing(voters.as_ref(), &expected, &vouched, sender, peer)
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
        let vouchers = askable_voters(&raft, &self.expected, Some(sender)).await;
        let asked: Vec<u64> = vouchers.iter().map(|(id, _)| *id).collect();
        let answers = ask_voters(&dialer, vouchers).await;
        let voucher = answers.into_iter().find_map(|(id, roster)| {
            let name = roster?.get(&sender)?.name.clone();
            peer.speaks_for(&name).then_some((id, name))
        });
        let Some((voucher, name)) = voucher else {
            eprintln!(
                "raft: refused a Raft RPC from node {sender} ({peer:?}): it isn't a voter in \
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

impl RaftAdmission {
    /// Keep the expected council current until `shutdown`: every
    /// [`EXPECTED_COUNCIL_REFRESH`], from this node's own membership when it
    /// has one, otherwise from what the voters it expects name now. A
    /// seatless node follows a changing council this way, one step at a
    /// time, as long as one voter it knows is still answering.
    pub async fn maintain_expected_council(
        self,
        raft: Raft<TypeConfig>,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        loop {
            if let Err(error) = self.refresh_expected_council(&raft).await {
                eprintln!("raft: could not save the expected council: {error}");
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(EXPECTED_COUNCIL_REFRESH) => {}
            }
        }
    }

    /// One refresh of the expected council (see
    /// [`Self::maintain_expected_council`]).
    pub async fn refresh_expected_council(&self, raft: &Raft<TypeConfig>) -> std::io::Result<()> {
        if let Some(roster) = council_roster(raft).await {
            return self.expected.replace(roster).await;
        }
        let Some(dialer) = &self.dialer else {
            return Ok(());
        };
        let voters = askable_voters(raft, &self.expected, None).await;
        if voters.is_empty() {
            return Ok(());
        }
        // Every voter that answered is one this node already trusts; the
        // voters any of them names now are the council to expect.
        let mut roster = CouncilRoster::new();
        for (_, answer) in ask_voters(dialer, voters).await {
            if let Some(answer) = answer {
                roster.extend(answer);
            }
        }
        self.expected.replace(roster).await
    }

    /// Ask the voters of this node's council (or of the council it expects),
    /// and those of the councils its recoveries replaced, which councils they
    /// serve, so that one that serves a newer recovery
    /// epoch tells this node so, and its fence moves (#2406). The answers
    /// themselves aren't needed: a refusal carries the epoch, and the
    /// transport hands that to the fence.
    pub async fn ask_voters_for_their_epoch(&self, raft: &Raft<TypeConfig>) {
        let Some(dialer) = &self.dialer else {
            return;
        };
        let me = raft.metrics().borrow().id;
        let mut voters: BTreeMap<u64, CouncilNodeInfo> = askable_voters(raft, &self.expected, None)
            .await
            .into_iter()
            .collect();
        for (id, info) in self.previous.iter() {
            if *id != me {
                voters.entry(*id).or_insert_with(|| info.clone());
            }
        }
        ask_voters(dialer, voters.into_iter().collect()).await;
    }
}

/// Ask each of `voters` for its council, over the node-id-bound transport,
/// each within [`VOUCH_TIMEOUT`]. `None` for a voter that didn't answer.
async fn ask_voters(
    dialer: &TcpRaftNetworkFactory,
    voters: Vec<(u64, CouncilNodeInfo)>,
) -> Vec<(u64, Option<CouncilRoster>)> {
    futures_util::future::join_all(voters.into_iter().map(|(id, info)| {
        let mut dialer = dialer.clone();
        async move {
            let network = dialer.new_client(id, &info).await;
            let answer = tokio::time::timeout(VOUCH_TIMEOUT, network.council_voters()).await;
            (id, answer.ok().and_then(Result::ok))
        }
    }))
    .await
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

    /// Whether a stranger sending this RPC is worth asking the council's
    /// voters about, given the index of this node's last log entry.
    ///
    /// Replication always is: this node may be the one that is behind. So
    /// is a vote whose log reaches at least as far as this node's: a voter
    /// that missed a membership change may be the vote a new voter needs to
    /// win, with the old leader gone. A vote with a shorter log isn't, since
    /// Raft would refuse it anyway.
    pub fn worth_vouching_for(&self, our_last_index: Option<u64>) -> bool {
        match self {
            RaftRpc::AppendEntries(_) | RaftRpc::InstallSnapshot(_) => true,
            RaftRpc::Vote(request) => request.last_log_id.map(|id| id.index) >= our_last_index,
            RaftRpc::CouncilVoters => false,
        }
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

    /// A node with no council of its own serves only the council it expects
    /// (#2397): not a node certificate that names itself leader, and not
    /// one speaking for an expected voter.
    #[test]
    fn a_node_with_no_council_serves_only_the_council_it_expects() {
        let none = CouncilVoters::new();
        assert_ne!(
            standing(None, &none, &none, Some(5), &node("n5")),
            Standing::NoCouncil,
            "an mTLS node that expects nobody must serve nobody"
        );
        let expected = voters(&[(1, "n1"), (2, "n2")]);
        assert_eq!(
            standing(None, &expected, &none, Some(1), &node("n1")),
            Standing::NoCouncil
        );
        assert_eq!(
            standing(None, &expected, &none, Some(9), &node("n9")),
            Standing::Stranger
        );
        assert_eq!(
            standing(None, &expected, &none, Some(1), &node("n9")),
            Standing::Stranger
        );
        let vouched = voters(&[(3, "n3")]);
        assert_eq!(
            standing(None, &expected, &vouched, Some(3), &node("n3")),
            Standing::NoCouncil
        );
    }

    /// A plaintext cluster has no certificates and no join, so a node there
    /// with nothing to expect still serves whoever replicates to it; once it
    /// expects a council, it holds the RPC's claim to it.
    #[test]
    fn a_plaintext_node_with_no_council_expects_what_it_was_told() {
        let none = CouncilVoters::new();
        assert_eq!(
            standing(None, &none, &none, Some(9), &PeerIdentity::Plaintext),
            Standing::NoCouncil
        );
        let expected = voters(&[(1, "n1")]);
        assert_eq!(
            standing(None, &expected, &none, Some(9), &PeerIdentity::Plaintext),
            Standing::Stranger
        );
    }

    #[test]
    fn a_voter_presenting_its_own_certificate_is_a_member() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, &none, Some(2), &node("n2")),
            Standing::Member
        );
    }

    #[test]
    fn a_node_outside_the_council_is_a_stranger() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, &none, Some(9), &node("n9")),
            Standing::Stranger
        );
    }

    #[test]
    fn a_certificate_cannot_speak_for_another_voter() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, &none, Some(2), &node("n9")),
            Standing::Stranger
        );
        assert_eq!(
            standing(
                Some(&council),
                &none,
                &none,
                Some(2),
                &PeerIdentity::Unnamed
            ),
            Standing::Stranger
        );
    }

    #[test]
    fn an_rpc_that_names_no_sender_is_a_strangers() {
        let council = voters(&[(1, "n1")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, &none, None, &node("n1")),
            Standing::Stranger
        );
    }

    #[test]
    fn plaintext_trusts_the_rpcs_claim_but_still_checks_the_council() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(
                Some(&council),
                &none,
                &none,
                Some(2),
                &PeerIdentity::Plaintext
            ),
            Standing::Member
        );
        assert_eq!(
            standing(
                Some(&council),
                &none,
                &none,
                Some(9),
                &PeerIdentity::Plaintext
            ),
            Standing::Stranger
        );
    }

    #[test]
    fn a_vouched_sender_is_a_member_under_the_vouched_name_only() {
        let council = voters(&[(1, "n1"), (2, "n2")]);
        let vouched = voters(&[(3, "n3")]);
        let none = CouncilVoters::new();
        assert_eq!(
            standing(Some(&council), &none, &vouched, Some(3), &node("n3")),
            Standing::Member
        );
        assert_eq!(
            standing(Some(&council), &none, &vouched, Some(3), &node("n9")),
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
        let vote = RaftRpc::Vote(VoteRequest::new(openraft::Vote::new(3, 5), None));
        assert_eq!(vote.sender(), Some(5));
        assert_eq!(RaftRpc::CouncilVoters.sender(), None);
    }

    fn vote_with_last_index(index: Option<u64>) -> RaftRpc {
        use openraft::raft::VoteRequest;
        RaftRpc::Vote(VoteRequest::new(
            openraft::Vote::new(3, 5),
            index.map(|index| openraft::LogId::new(openraft::CommittedLeaderId::new(2, 5), index)),
        ))
    }

    /// Replication from a stranger is always worth asking about: this node
    /// may be the one that is behind. A stranger's vote is worth asking
    /// about only when its log reaches at least as far as this node's,
    /// because Raft would refuse a shorter log's candidate anyway.
    #[test]
    fn a_strangers_replication_or_long_enough_vote_is_worth_asking_about() {
        use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest};
        let append = RaftRpc::AppendEntries(AppendEntriesRequest {
            vote: openraft::Vote::new_committed(3, 7),
            prev_log_id: None,
            entries: Vec::new(),
            leader_commit: None,
        });
        assert!(append.worth_vouching_for(Some(10)));
        let snapshot = RaftRpc::InstallSnapshot(InstallSnapshotRequest {
            vote: openraft::Vote::new_committed(3, 7),
            meta: Default::default(),
            offset: 0,
            data: Vec::new(),
            done: false,
        });
        assert!(snapshot.worth_vouching_for(Some(10)));

        assert!(vote_with_last_index(Some(12)).worth_vouching_for(Some(10)));
        assert!(vote_with_last_index(Some(10)).worth_vouching_for(Some(10)));
        assert!(!vote_with_last_index(Some(9)).worth_vouching_for(Some(10)));
        assert!(!vote_with_last_index(None).worth_vouching_for(Some(0)));
        assert!(vote_with_last_index(None).worth_vouching_for(None));
        assert!(!RaftRpc::CouncilVoters.worth_vouching_for(None));
    }
}

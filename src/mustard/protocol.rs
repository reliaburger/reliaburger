/// SWIM probe cycle protocol.
///
/// Each protocol period, the node:
/// 1. Picks a random alive peer to probe.
/// 2. Sends a PING (with piggybacked membership updates).
/// 3. Waits for an ACK within `probe_timeout`.
/// 4. If no ACK, sends PING-REQ to up to `indirect_probe_count` random peers.
/// 5. Waits one more `probe_timeout` for a relayed or late direct ACK
///    (even when no relay exists), then marks the target as Suspect.
/// 6. Promotes expired suspects to Dead.
///
/// Every `push_pull_interval` (and on the first cycle, so a joining node
/// syncs straight away) it also exchanges its membership table with one
/// random live peer: anti-entropy for whatever piggybacking missed.
///
/// The `MustardNode` struct owns the membership table, dissemination
/// queue, and transport, and drives the protocol as an async task.
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use tokio_util::sync::CancellationToken;

use tokio::sync::watch;

use crate::meat::NodeId;

use super::config::GossipConfig;
use super::directory::NodeDirectory;
use super::dissemination::DisseminationQueue;
use super::membership::{MembershipSnapshot, MembershipTable};
use super::message::{
    DirectoryExtension, GossipMessage, GossipPayload, LeaderHint, MAX_PIGGYBACK_UPDATES,
    MAX_SYNC_ENTRIES, MembershipUpdate,
};
use super::state::NodeState;
use super::transport::MustardTransport;

/// What a published membership snapshot is compared by.
type MembershipDigest = (
    NodeId,
    NodeState,
    u64,
    bool,
    bool,
    SocketAddr,
    BTreeMap<String, String>,
);

/// A participant in the Mustard gossip protocol.
///
/// Owns the membership table, dissemination queue, and transport.
/// Call `run()` to start the protocol loop, or `run_one_cycle()` to
/// drive a single probe period (useful for testing).
pub struct MustardNode<T: MustardTransport> {
    /// This node's identity.
    pub node_id: NodeId,
    /// This node's cluster address.
    pub address: SocketAddr,
    /// This node's incarnation number (bumped on refutation).
    pub incarnation: u64,
    /// Cluster membership.
    pub membership: MembershipTable,
    /// Pending updates to piggyback on outgoing messages.
    pub dissemination: DisseminationQueue,
    /// Protocol configuration.
    pub config: GossipConfig,
    /// Network transport.
    pub transport: T,
    /// Optional watch channel for publishing membership snapshots.
    /// Set when running inside the agent, None in standalone tests.
    membership_watch: Option<watch::Sender<Vec<MembershipSnapshot>>>,
    /// Process-local proof that a peer has acknowledged our gossip.
    rejoin_watch: Option<watch::Sender<bool>>,
    /// Digest of the last-published membership. Used to publish on any content
    /// change (state/incarnation/council/leader), not just a count change.
    last_published_digest: Vec<MembershipDigest>,
    /// Whether this node has announced its own departure.
    ///
    /// Once it has, a `Left` about us echoing back off a peer is our own
    /// announcement coming home, not a replay to refute — resurrecting
    /// ourselves there would undo a deliberate shutdown (O9).
    left: bool,
    /// Seed addresses to (re-)contact while this node has no live peers.
    /// Used to bootstrap a join by address without inserting a placeholder
    /// member: we ping the seed, and learn its real identity from the reply.
    seeds: Vec<SocketAddr>,
    /// Bounded, previously contacted peers retained after dead-member reaping.
    /// Bootstrap has no configured seeds, but still needs a way back after isolation.
    rejoin_contacts: VecDeque<(NodeId, SocketAddr)>,
    /// This node's advertised control-plane endpoints (API, reporting),
    /// stamped on every outgoing datagram. `None` until the runtime wires
    /// them in — then no extension is sent (pre-12b.2 behaviour).
    advertised: Option<(SocketAddr, SocketAddr)>,
    /// This node's placement labels, advertised (bounded) on every
    /// datagram alongside the endpoints so remote members learn them
    /// (CP7). Empty until the runtime wires them in.
    advertised_labels: BTreeMap<String, String>,
    /// The local leader hint: `Some` only while THIS node is the Raft
    /// leader (published by the cluster runtime). Everyone else relays the
    /// best hint they have heard instead.
    leader_hint_rx: Option<watch::Receiver<Option<LeaderHint>>>,
    /// This node's own sustained-disk-pressure verdict, published by the
    /// cluster runtime (12b.2 T3). `true` stamps `disk_pressured` on every
    /// outgoing extension so the leader learns this voter should resign.
    /// `None` until wired — then the bit is always `false`.
    disk_pressured_rx: Option<watch::Receiver<bool>>,
    /// Live council voter set and leader by name, published by the cluster
    /// runtime from Raft metrics. Applied to the membership table each publish
    /// cycle so `is_council`/`is_leader` are correct. `None` on a single-node
    /// process, where the flags stay `false`.
    council_roles_rx: Option<watch::Receiver<super::membership::CouncilRoles>>,
    /// Directory accumulated from received extensions.
    directory: NodeDirectory,
    /// Optional watch channel publishing the directory on change.
    directory_watch: Option<watch::Sender<NodeDirectory>>,
    /// The Smoker node-kill switch shared with this node's transports.
    /// While it is closed this node is "dead": it forms no opinions about
    /// its peers, so it has no stale suspicions to spread once it reopens.
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    /// Source of randomness for probe-target and relay selection. Seeded
    /// from the OS in production; tests can pin it to replay one schedule.
    rng: StdRng,
    /// When the next anti-entropy push-pull is due. Starts at creation, so
    /// the first cycle with a live peer syncs: that is the join sync.
    next_push_pull: Instant,
    /// Where the next outgoing window of the membership table starts, for
    /// tables larger than one exchange carries (`MAX_SYNC_ENTRIES`).
    sync_cursor: usize,
}

impl<T: MustardTransport> MustardNode<T> {
    /// Publish fresh process-local rejoin evidence after a direct peer ACK.
    /// Seed entries and piggybacked membership never count as proof.
    pub fn set_rejoin_watch(&mut self, sender: watch::Sender<bool>) {
        self.rejoin_watch = Some(sender);
    }

    /// Maximum number of peers to notify during graceful leave.
    const MAX_LEAVE_FANOUT: usize = 10;
    const MAX_REJOIN_CONTACTS: usize = 16;

    /// Create a new Mustard node.
    pub fn new(node_id: NodeId, address: SocketAddr, config: GossipConfig, transport: T) -> Self {
        let mut membership = MembershipTable::new();
        // Register ourselves
        membership.add_node(node_id.clone(), address, 1, BTreeMap::new(), Instant::now());

        Self {
            node_id,
            address,
            incarnation: 1,
            membership,
            dissemination: DisseminationQueue::new(),
            config,
            transport,
            membership_watch: None,
            rejoin_watch: None,
            last_published_digest: Vec::new(),
            left: false,
            seeds: Vec::new(),
            rejoin_contacts: VecDeque::new(),
            advertised: None,
            advertised_labels: BTreeMap::new(),
            leader_hint_rx: None,
            disk_pressured_rx: None,
            council_roles_rx: None,
            directory: NodeDirectory::default(),
            directory_watch: None,
            node_gate: crate::smoker::node_fault::NodeTransportGate::new(),
            rng: StdRng::from_entropy(),
            next_push_pull: Instant::now(),
            sync_cursor: 0,
        }
    }

    /// Share the Smoker node-kill switch that also gates this node's
    /// transports, so failure detection pauses while the node plays dead.
    pub fn set_node_gate(&mut self, gate: crate::smoker::node_fault::NodeTransportGate) {
        self.node_gate = gate;
    }

    /// Advertise this node's control-plane endpoints (API and reporting
    /// ports on the gossip IP) and placement labels. From here on, every
    /// outgoing datagram carries a directory extension, and the local
    /// directory and membership resolve this node's own endpoints and
    /// labels. Labels are bounded on the wire (see
    /// [`bounded_labels`](super::message::bounded_labels)); the local copy
    /// keeps the full set.
    pub fn set_advertised_endpoints(
        &mut self,
        api_port: u16,
        reporting_port: u16,
        labels: BTreeMap<String, String>,
    ) {
        let api_address = SocketAddr::new(self.address.ip(), api_port);
        let reporting_address = SocketAddr::new(self.address.ip(), reporting_port);
        self.advertised = Some((api_address, reporting_address));
        self.advertised_labels = labels.clone();
        self.directory.endpoints.insert(
            self.node_id.clone(),
            super::directory::NodeEndpoints {
                api_address,
                reporting_address,
            },
        );
        self.directory
            .labels
            .insert(self.node_id.clone(), labels.clone());
        // Keep our own membership record's labels in sync so a snapshot of
        // the local table already carries them (the scheduler reads labels
        // from the membership snapshot).
        if let Some(member) = self.membership.get_mut(&self.node_id) {
            member.labels = labels;
        }
        self.publish_directory();
    }

    /// Wire the local leader hint: the cluster runtime publishes
    /// `Some(hint)` while this node is the Raft leader, `None` otherwise.
    pub fn set_leader_hint_watch(&mut self, rx: watch::Receiver<Option<LeaderHint>>) {
        self.leader_hint_rx = Some(rx);
    }

    /// Wire this node's own disk-pressure verdict: the cluster runtime
    /// publishes `true` once the node's disk has been over its threshold for
    /// the whole hold-down window, and every outgoing extension then advertises
    /// `disk_pressured` so the leader's reconciler can replace this voter.
    pub fn set_disk_pressured_watch(&mut self, rx: watch::Receiver<bool>) {
        self.disk_pressured_rx = Some(rx);
    }

    /// Wire the live council roles (voter set + leader by name) the cluster
    /// runtime derives from Raft metrics. Applied to the membership table on
    /// each publish so `is_council`/`is_leader` reflect reality. Without a
    /// producer the flags stay `false`, the pre-wiring behaviour.
    pub fn set_council_roles_watch(
        &mut self,
        rx: watch::Receiver<super::membership::CouncilRoles>,
    ) {
        self.council_roles_rx = Some(rx);
    }

    /// Set the directory watch channel, publishing endpoint and leader-hint
    /// changes learned from gossip.
    pub fn set_directory_watch(&mut self, tx: watch::Sender<NodeDirectory>) {
        let _ = tx.send(self.directory.clone());
        self.directory_watch = Some(tx);
    }

    /// Read access to the accumulated directory (mainly for tests).
    pub fn directory(&self) -> &NodeDirectory {
        &self.directory
    }

    /// Build the extension for an outgoing datagram: our advertised
    /// endpoints plus the best leader hint we can offer — our own if we
    /// lead (it carries the freshest term), otherwise the best relayed one.
    fn local_extension(&self) -> Option<DirectoryExtension> {
        let (api_address, reporting_address) = self.advertised?;
        let own = self
            .leader_hint_rx
            .as_ref()
            .and_then(|rx| rx.borrow().clone());
        let relayed = self.directory.leader.clone();
        let leader = match (own, relayed) {
            (Some(a), Some(b)) => Some(if a.term >= b.term { a } else { b }),
            (a, b) => a.or(b),
        };
        let disk_pressured = self
            .disk_pressured_rx
            .as_ref()
            .map(|rx| *rx.borrow())
            .unwrap_or(false);
        Some(DirectoryExtension {
            node_id: self.node_id.clone(),
            api_address,
            reporting_address,
            leader,
            labels: super::message::bounded_labels(&self.advertised_labels),
            disk_pressured,
            hmac: [0u8; 32],
        })
    }

    /// Stamp an outgoing message with this node's directory extension.
    fn stamp(&self, mut message: GossipMessage) -> GossipMessage {
        message.extension = self.local_extension();
        message
    }

    /// Fold a received extension into the directory and publish on change.
    /// The extension's labels are also mirrored onto the stamping node's
    /// membership record, so a membership snapshot carries them for the
    /// scheduler (a `MembershipUpdate` never has).
    fn ingest_extension(&mut self, extension: &DirectoryExtension) {
        let changed = self.directory.observe(extension);
        if let Some(member) = self.membership.get_mut(&extension.node_id)
            && member.labels != extension.labels
        {
            member.labels = extension.labels.clone();
        }
        if changed {
            self.publish_directory();
        }
    }

    fn publish_directory(&self) {
        if let Some(tx) = &self.directory_watch {
            let _ = tx.send(self.directory.clone());
        }
    }

    /// Set seed addresses used to bootstrap a join.
    ///
    /// While this node knows no live peers, each probe cycle pings these
    /// addresses directly. We never insert a placeholder member for a seed:
    /// the seed's reply carries its real `NodeId`, so membership stays free
    /// of phantom entries even though we joined knowing only an address.
    pub fn set_seeds(&mut self, seeds: Vec<SocketAddr>) {
        self.seeds = seeds;
    }

    /// Probe configured seeds while isolated.
    async fn ping_seeds(&mut self) {
        let addresses = self.seeds.clone();
        for address in addresses {
            let peer = self
                .membership
                .iter()
                .find(|member| member.address == address)
                .map(|member| member.node_id.clone());
            let updates = self.updates_for_peer(peer.as_ref());
            let ping = GossipMessage::new(
                self.node_id.clone(),
                self.incarnation,
                GossipPayload::Ping { updates },
            );
            let _ = self.transport.send(address, &self.stamp(ping)).await;
        }
    }

    /// Repair partial membership loss as well as complete isolation. One
    /// retained contact per cycle bounds traffic without depending on which
    /// other peers are currently alive.
    async fn ping_rejoin_contact(&mut self) {
        let Some((node, address)) = self.rejoin_contacts.pop_front() else {
            return;
        };
        self.rejoin_contacts.push_back((node.clone(), address));
        if self
            .membership
            .get(&node)
            .is_some_and(|member| member.state == NodeState::Alive)
        {
            return;
        }
        let updates = self.updates_for_peer(Some(&node));
        let ping = GossipMessage::new(
            self.node_id.clone(),
            self.incarnation,
            GossipPayload::Ping { updates },
        );
        let _ = self.transport.send(address, &self.stamp(ping)).await;
    }

    /// A directly contacted peer must learn our current suspicion/death claim
    /// even if its ordinary piggyback retransmissions have been exhausted.
    fn updates_for_peer(&mut self, peer: Option<&NodeId>) -> Vec<MembershipUpdate> {
        let mut updates = self.dissemination.select_updates();
        if let Some(member) = peer.and_then(|peer| self.membership.get(peer))
            && member.state != NodeState::Alive
        {
            updates.retain(|update| update.node_id != member.node_id);
            updates.insert(
                0,
                MembershipUpdate {
                    node_id: member.node_id.clone(),
                    address: member.address,
                    state: member.state,
                    incarnation: member.incarnation,
                },
            );
            updates.truncate(super::message::MAX_PIGGYBACK_UPDATES);
        }
        updates
    }

    /// Set the membership watch channel for publishing snapshots.
    pub fn set_membership_watch(&mut self, tx: watch::Sender<Vec<MembershipSnapshot>>) {
        self.membership_watch = Some(tx);
    }

    /// Publish the current membership to the watch channel if its *content*
    /// changed. Comparing a digest (not just the member count) means state
    /// transitions like Alive→Suspect — which keep the count constant until the
    /// reap — are published promptly to the council reconciler and `relish nodes`.
    fn publish_membership(&mut self) {
        // Refresh council/leader flags from the latest Raft-derived roles before
        // snapshotting. Clone the value out first so the watch borrow is released
        // before taking `&mut self.membership` (different fields, but the borrow
        // checker can't see that through method calls).
        if let Some(roles) = self.council_roles_rx.as_ref().map(|rx| rx.borrow().clone()) {
            self.membership.set_roles(&roles);
        }
        let snapshot = self.membership.snapshot();
        // Labels and addresses change without a state or incarnation change
        // (a node restarted with a new `[node.labels]`), so they're part of
        // the digest too; otherwise that change is never published.
        let digest: Vec<MembershipDigest> = snapshot
            .iter()
            .map(|m| {
                (
                    m.node_id.clone(),
                    m.state,
                    m.incarnation,
                    m.is_council,
                    m.is_leader,
                    m.address,
                    m.labels.clone(),
                )
            })
            .collect();
        if digest != self.last_published_digest {
            if let Some(tx) = &self.membership_watch {
                let _ = tx.send(snapshot);
            }
            self.last_published_digest = digest;
        }
    }

    /// Add a seed node to bootstrap cluster discovery.
    pub fn add_seed(&mut self, node_id: NodeId, address: SocketAddr) {
        self.membership
            .add_node(node_id, address, 1, BTreeMap::new(), Instant::now());
    }

    /// Announce graceful departure from the cluster.
    ///
    /// Sets own state to Left, enqueues the update for dissemination,
    /// and sends a best-effort burst of PINGs to spread the update
    /// quickly. The node does not wait for acknowledgement.
    pub async fn leave(&mut self) {
        let now = Instant::now();
        self.left = true;

        // Mark ourselves as Left
        if let Some(member) = self.membership.get_mut(&self.node_id) {
            member.state = NodeState::Left;
            member.state_changed = now;
        }

        // Enqueue Left update for dissemination
        self.dissemination.enqueue(
            MembershipUpdate {
                node_id: self.node_id.clone(),
                address: self.address,
                state: NodeState::Left,
                incarnation: self.incarnation,
            },
            self.membership.len(),
        );

        // Best-effort fanout to accelerate propagation
        let peers: Vec<SocketAddr> = self
            .membership
            .alive_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id)
            .map(|m| m.address)
            .take(Self::MAX_LEAVE_FANOUT)
            .collect();

        for peer_addr in peers {
            let updates = self.dissemination.select_updates();
            let ping = GossipMessage::new(
                self.node_id.clone(),
                self.incarnation,
                GossipPayload::Ping { updates },
            );
            let _ = self.transport.send(peer_addr, &self.stamp(ping)).await;
        }
    }

    /// Run the protocol loop until cancelled.
    ///
    /// On shutdown, announces graceful departure via [`Self::leave()`] before
    /// returning, so other nodes learn about the departure immediately
    /// rather than waiting for the suspicion timeout.
    pub async fn run(&mut self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(self.config.protocol_interval);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.leave().await;
                    break;
                }
                _ = interval.tick() => {
                    self.run_one_cycle().await;
                    self.publish_membership();
                }
                msg = self.transport.recv() => {
                    if let Some((from, message)) = msg {
                        self.handle_message(from, message).await;
                        self.publish_membership();
                    }
                }
            }
        }
    }

    /// Run a single probe cycle.
    ///
    /// Picks a random peer, pings it, waits for ACK (or does indirect
    /// probing), and promotes expired suspects to dead. Exposed publicly
    /// so tests can drive the protocol step-by-step.
    pub async fn run_one_cycle(&mut self) {
        // A node-kill fault makes this node play dead. A dead process runs
        // no failure detector, and every probe would fail anyway, so any
        // verdict reached now would be about the gate, not the peer.
        if self.node_gate.is_quiesced() {
            return;
        }
        // An explicit departure retires a contact; a failure does not. Inspect
        // Left before reaping, while that distinction still exists.
        self.rejoin_contacts.retain(|(node, _)| {
            self.membership
                .get(node)
                .is_none_or(|member| member.state != NodeState::Left)
        });
        self.promote_expired_suspects();
        let now = Instant::now();
        let reaped = self
            .membership
            .reap_expired_dead(self.config.cleanup_timeout, now);
        if !reaped.is_empty() && self.directory.prune(&reaped) {
            self.publish_directory();
        }
        self.ping_rejoin_contact().await;

        // An isolated node stays due, so it syncs with the first peer it
        // finds rather than a whole interval later.
        if now >= self.next_push_pull && self.push_pull().await {
            self.next_push_pull = now + self.config.push_pull_interval;
        }

        let target = self.pick_probe_target();
        let Some((target_id, target_addr)) = target else {
            // Bootstrap may have no configured seeds. Remembered direct
            // contacts give it a route back even after every member is reaped.
            self.ping_seeds().await;
            return;
        };

        // Send PING
        let updates = self.dissemination.select_updates();
        let ping = GossipMessage::new(
            self.node_id.clone(),
            self.incarnation,
            GossipPayload::Ping { updates },
        );
        let _ = self.transport.send(target_addr, &self.stamp(ping)).await;

        // Wait for ACK
        let got_ack = self
            .wait_for_ack(&target_id, self.config.probe_timeout)
            .await;
        if got_ack {
            return;
        }

        // No direct ACK — try indirect probes
        let relays = self.pick_relays(&target_id);
        for relay_addr in &relays {
            let updates = self.dissemination.select_updates();
            let ping_req = GossipMessage::new(
                self.node_id.clone(),
                self.incarnation,
                GossipPayload::PingReq {
                    target: target_id.clone(),
                    requester: self.node_id.clone(),
                    updates,
                },
            );
            let _ = self
                .transport
                .send(*relay_addr, &self.stamp(ping_req))
                .await;
        }

        // Wait for an indirect ACK, or a late direct one. Wait even with no
        // relays: in a three-node cluster that has lost one member, the lost
        // member is the only possible relay, and suspecting the healthy peer
        // after a single `probe_timeout` would halve the evidence every other
        // probe gets. One slow ACK on a loaded host was enough.
        let got_late_or_indirect = self
            .wait_for_ack(&target_id, self.config.probe_timeout)
            .await;
        if got_late_or_indirect {
            return;
        }

        // No ACK at all — mark as suspect, unless the gate closed while we
        // waited: then the silence is ours, not the target's.
        if !self.node_gate.is_quiesced() && self.membership.suspect(&target_id) {
            self.dissemination.enqueue(
                MembershipUpdate {
                    node_id: target_id.clone(),
                    address: target_addr,
                    state: NodeState::Suspect,
                    // The suspicion is about `target_id`, so it must carry the
                    // target's incarnation — not the prober's (`ping.sender` is
                    // this node). A wrong incarnation is either discarded by
                    // peers (detection stops propagating) or wrongly overrides
                    // fresher Alive state.
                    incarnation: self.membership_incarnation_of(&target_id),
                },
                self.membership.len(),
            );
        }
    }

    /// Handle an incoming gossip message.
    pub async fn handle_message(&mut self, from: SocketAddr, message: GossipMessage) {
        if message.version != GossipMessage::VERSION
            || message.state_format != crate::compatibility::CURRENT.state
        {
            return;
        }
        let now = Instant::now();

        // Fold the directory extension in first — endpoint knowledge and the
        // leader hint are useful even if the membership payload is stale.
        if let Some(extension) = &message.extension {
            self.ingest_extension(extension);
        }

        // Register the sender at the socket it came from — but NOT for a
        // relayed ACK, whose socket is the relay's, not the sender's (M13).
        // Recording the probed node at the relay's address made the next
        // direct probe hit the relay, whose ACK (sender = relay ≠ probed node)
        // never matched, falsely evicting a healthy node. The relayed sender is
        // already the probe target we know; its liveness is applied below.
        let is_relayed_ack = matches!(&message.payload, GossipPayload::Ack { relayed: true, .. });
        let is_new = if is_relayed_ack {
            false
        } else {
            if message.sender != self.node_id && from != self.address {
                self.rejoin_contacts
                    .retain(|(node, _)| node != &message.sender);
                self.rejoin_contacts
                    .push_back((message.sender.clone(), from));
                if self.rejoin_contacts.len() > Self::MAX_REJOIN_CONTACTS {
                    self.rejoin_contacts.pop_front();
                }
            }
            self.membership.add_node(
                message.sender.clone(),
                from,
                message.incarnation,
                BTreeMap::new(),
                now,
            )
        };

        // Mirror the freshly-registered sender's advertised labels onto its
        // membership record. `ingest_extension` above ran before `add_node`,
        // so a first-contact sender wouldn't yet have a record to label;
        // do it here now the record exists.
        if let Some(extension) = &message.extension
            && let Some(member) = self.membership.get_mut(&extension.node_id)
            && member.labels != extension.labels
        {
            member.labels = extension.labels.clone();
        }

        // Disseminate newly discovered nodes so the whole cluster learns
        if is_new {
            self.dissemination.enqueue(
                MembershipUpdate {
                    node_id: message.sender.clone(),
                    address: from,
                    state: NodeState::Alive,
                    incarnation: message.incarnation,
                },
                self.membership.len(),
            );
        }

        // Process piggybacked updates
        for update in message.payload.updates() {
            let changed = self.membership.apply_update(update, now);
            if changed {
                // Re-disseminate to other nodes
                self.dissemination
                    .enqueue(update.clone(), self.membership.len());
            }

            // If we're being suspected, declared dead, or reported as having
            // left, refute it. Refuting Dead matters as much as Suspect:
            // without it a false Dead about us is unrecoverable until the 60s
            // reap (we'd be invisible to scheduling and the council the whole
            // time). A higher incarnation resurrects us — see
            // `resolve_conflict`.
            //
            // Left is here for the same reason (O9). We only ever announce our
            // own departure on the way down, so a Left about us arriving at a
            // running node is a replay of an older one; refuting it at a higher
            // incarnation is the only way back, and it's a route nobody else
            // can take — peers can't mint our incarnation numbers.
            let refutable = match update.state {
                NodeState::Suspect | NodeState::Dead => true,
                NodeState::Left => !self.left,
                NodeState::Alive => false,
            };
            if update.node_id == self.node_id && refutable {
                self.refute(update.incarnation);
            }
        }

        // Handle the message type
        match &message.payload {
            GossipPayload::Ping { .. } => {
                // A returning peer can refute a retained Dead claim even when
                // ordinary dissemination has already forgotten that update.
                let updates = self.updates_for_peer(Some(&message.sender));
                let ack = GossipMessage::new(
                    self.node_id.clone(),
                    self.incarnation,
                    GossipPayload::Ack {
                        updates,
                        relayed: false,
                    },
                );
                let _ = self.transport.send(from, &self.stamp(ack)).await;
            }
            GossipPayload::PingReq {
                target, requester, ..
            } => {
                // Probe the target on behalf of the requester.
                // If the target responds, forward an ACK to the requester
                // with sender = target's NodeId so the requester's
                // wait_for_ack recognises it.
                if let Some(target_member) = self.membership.get(target) {
                    let target_addr = target_member.address;
                    let requester = requester.clone();
                    let target = target.clone();
                    let updates = self.dissemination.select_updates();
                    let ping = GossipMessage::new(
                        self.node_id.clone(),
                        self.incarnation,
                        GossipPayload::Ping { updates },
                    );
                    let _ = self.transport.send(target_addr, &self.stamp(ping)).await;

                    // Wait for target's ACK (simple inline wait to avoid
                    // async recursion through handle_message → wait_for_ack)
                    let got_ack = self
                        .wait_for_relay_ack(&target, self.config.probe_timeout)
                        .await;

                    if got_ack {
                        // Forward ACK to the original requester
                        if let Some(req_member) = self.membership.get(&requester) {
                            let req_addr = req_member.address;
                            let target_inc = self.membership_incarnation_of(&target);
                            let fwd_updates = self.dissemination.select_updates();
                            let fwd_ack = GossipMessage::new(
                                target,
                                target_inc,
                                GossipPayload::Ack {
                                    updates: fwd_updates,
                                    // Forwarded on the target's behalf: the
                                    // requester must not learn the target's
                                    // address from our socket (M13).
                                    relayed: true,
                                },
                            );
                            let _ = self.transport.send(req_addr, &self.stamp(fwd_ack)).await;
                        }
                    }
                }
            }
            GossipPayload::Sync { wants_reply, .. } => {
                // The entries were merged above like any piggybacked update.
                // Answer a request with our own table; never answer a reply,
                // or two nodes would bounce their tables back and forth.
                if *wants_reply {
                    self.send_membership(from, false).await;
                }
            }
            GossipPayload::Ack { relayed, .. } => {
                if !relayed
                    && message.sender != self.node_id
                    && let Some(sender) = &self.rejoin_watch
                {
                    sender.send_replace(true);
                }
                // Mark sender as alive (ACK received)
                if let Some(member) = self.membership.get_mut(&message.sender) {
                    if member.state == NodeState::Suspect {
                        member.state = NodeState::Alive;
                        member.state_changed = now;
                    }
                    member.last_ack = now;
                }
            }
        }
    }

    /// Bump incarnation and disseminate an Alive update to refute suspicion.
    /// Refute a Suspect/Dead claim about this node by broadcasting a fresh
    /// Alive at a higher incarnation.
    ///
    /// `offending_incarnation` is the incarnation of the claim being refuted.
    /// We jump *past* it — `max(local, offending) + 1` — rather than merely
    /// `local + 1` (M12). A node that reached incarnation 50, crashed while
    /// peers held it Dead@50, and restarted at 1 would otherwise need ~49
    /// refutes (often longer than the 60s reap) before its Alive out-ranks the
    /// stale Dead. Seeding from the seen value means a single refute wins.
    fn refute(&mut self, offending_incarnation: u64) {
        self.incarnation = self.incarnation.max(offending_incarnation) + 1;
        let update = MembershipUpdate {
            node_id: self.node_id.clone(),
            address: self.address,
            state: NodeState::Alive,
            incarnation: self.incarnation,
        };
        // Apply the refutation to our own record too, not just the outbound
        // queue (O9). The claim we're refuting was applied a moment ago, so
        // without this a node tells the cluster it's Alive while its own
        // membership table — the one its scheduler and council reads see —
        // still holds it Suspect, Dead or Left.
        self.membership.apply_update(&update, Instant::now());
        self.dissemination.enqueue(update, self.membership.len());
    }

    /// Start an anti-entropy exchange with one random live peer.
    ///
    /// Pushes our membership table (or the next window of it, see
    /// `membership_window`) and asks the peer to push its own back.
    /// Both sides merge under the ordinary SWIM precedence, so a stale entry
    /// can't resurrect a dead node or undo a refutation. Returns `false` when
    /// there is no live peer to sync with.
    pub async fn push_pull(&mut self) -> bool {
        let mut candidates: Vec<_> = self
            .membership
            .alive_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id)
            .map(|m| (m.node_id.clone(), m.address))
            .collect();
        candidates.sort_unstable();
        let Some((_, address)) = candidates.choose(&mut self.rng).cloned() else {
            return false;
        };
        self.send_membership(address, true).await;
        true
    }

    /// Send our membership table to `target` as a burst of `Sync` datagrams.
    /// Only the first datagram carries `wants_reply`, so the peer answers once.
    ///
    /// Sync datagrams go out unstamped: the directory extension can take
    /// ~700 bytes with a full label set, and every probe already carries it,
    /// so the whole UDP budget goes to membership entries instead.
    async fn send_membership(&mut self, target: SocketAddr, wants_reply: bool) {
        let window = self.membership_window();
        for (index, entries) in window.chunks(MAX_PIGGYBACK_UPDATES).enumerate() {
            let sync = GossipMessage::new(
                self.node_id.clone(),
                self.incarnation,
                GossipPayload::Sync {
                    entries: entries.to_vec(),
                    wants_reply: wants_reply && index == 0,
                },
            );
            let _ = self.transport.send(target, &sync).await;
        }
    }

    /// The membership entries for one exchange: the whole table when it fits
    /// in [`MAX_SYNC_ENTRIES`], otherwise the next window of that many,
    /// rotating through the table (sorted by node id) one exchange at a time.
    ///
    /// Every entry goes, whatever its state. A peer that missed a `Dead` or
    /// `Left` needs to hear it as much as one that missed a join.
    fn membership_window(&mut self) -> Vec<MembershipUpdate> {
        let mut entries: Vec<MembershipUpdate> = self
            .membership
            .iter()
            .map(|m| MembershipUpdate {
                node_id: m.node_id.clone(),
                address: m.address,
                state: m.state,
                incarnation: m.incarnation,
            })
            .collect();
        if entries.len() <= MAX_SYNC_ENTRIES {
            return entries;
        }
        entries.sort_unstable_by(|a, b| a.node_id.cmp(&b.node_id));
        let start = self.sync_cursor % entries.len();
        self.sync_cursor = start + MAX_SYNC_ENTRIES;
        entries
            .iter()
            .cycle()
            .skip(start)
            .take(MAX_SYNC_ENTRIES)
            .cloned()
            .collect()
    }

    /// Promote suspects whose suspicion timeout has expired to Dead.
    fn promote_expired_suspects(&mut self) {
        let timeout = self.config.suspicion_timeout;
        let now = Instant::now();
        let mut newly_dead = Vec::new();

        for member in self.membership.iter() {
            // Measure from when suspicion *started* (`state_changed`), not from
            // the last ACK. For a gossip-learned peer `last_ack` can be
            // arbitrarily stale, which would promote it to Dead on the first
            // failed probe — skipping the refutation window entirely.
            if member.state == NodeState::Suspect
                && member.node_id != self.node_id
                && now.duration_since(member.state_changed) > timeout
            {
                newly_dead.push((member.node_id.clone(), member.address));
            }
        }

        for (node_id, node_addr) in newly_dead {
            if self.membership.declare_dead(&node_id) {
                let inc = self.membership_incarnation_of(&node_id);
                self.dissemination.enqueue(
                    MembershipUpdate {
                        node_id,
                        address: node_addr,
                        state: NodeState::Dead,
                        incarnation: inc,
                    },
                    self.membership.len(),
                );
            }
        }
    }

    /// Pick a random alive peer to probe (not ourselves).
    pub fn pick_probe_target(&mut self) -> Option<(NodeId, SocketAddr)> {
        let mut candidates: Vec<_> = self
            .membership
            .active_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id)
            .collect();
        // The table is a HashMap, whose order differs per process; sorting
        // makes the choice depend only on the RNG, so a seeded RNG replays.
        candidates.sort_unstable_by(|a, b| a.node_id.cmp(&b.node_id));

        let target = candidates.choose(&mut self.rng)?;
        Some((target.node_id.clone(), target.address))
    }

    /// Pick random relay nodes for indirect probing (not ourselves, not the target).
    fn pick_relays(&mut self, target: &NodeId) -> Vec<SocketAddr> {
        let mut candidates: Vec<_> = self
            .membership
            .alive_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id && m.node_id != *target)
            .collect();
        candidates.sort_unstable_by(|a, b| a.node_id.cmp(&b.node_id));

        let count = self.config.indirect_probe_count.min(candidates.len());
        candidates
            .choose_multiple(&mut self.rng, count)
            .map(|m| m.address)
            .collect()
    }

    /// Replace the OS-seeded RNG with a fixed seed, so a test replays
    /// exactly one probe schedule.
    #[cfg(test)]
    fn seed_rng(&mut self, seed: u64) {
        self.rng = StdRng::seed_from_u64(seed);
    }

    /// Wait for an ACK from (or about) the target within the timeout.
    ///
    /// Drains inbound messages while waiting. Non-ACK messages are
    /// still handled (their piggybacked updates are applied).
    async fn wait_for_ack(&mut self, target_id: &NodeId, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }

            match tokio::time::timeout(remaining, self.transport.recv()).await {
                Ok(Some((from, message))) => {
                    let is_ack_from_target = matches!(&message.payload, GossipPayload::Ack { .. })
                        && message.sender == *target_id;

                    self.handle_message(from, message).await;

                    if is_ack_from_target {
                        return true;
                    }
                }
                Ok(None) => return false, // Transport shut down
                Err(_) => return false,   // Timeout
            }
        }
    }

    /// Wait for an ACK from the target during a relay probe.
    ///
    /// Unlike `wait_for_ack`, this does not recursively call `handle_message`
    /// (which would cause async recursion). It only checks for ACKs and
    /// applies piggybacked updates from them.
    async fn wait_for_relay_ack(
        &mut self,
        target_id: &NodeId,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }

            match tokio::time::timeout(remaining, self.transport.recv()).await {
                Ok(Some((_from, message))) => {
                    let is_ack_from_target = matches!(&message.payload, GossipPayload::Ack { .. })
                        && message.sender == *target_id;

                    // Apply piggybacked updates without full handle_message
                    let now = Instant::now();
                    if let Some(extension) = &message.extension {
                        self.ingest_extension(extension);
                    }
                    for update in message.payload.updates() {
                        self.membership.apply_update(update, now);
                        // Refute a Suspect/Dead/Left claimed about *us* here too
                        // (M11): this relay-wait applies piggybacked updates but
                        // used to skip the refutation `handle_message` does, so a
                        // false Suspect/Dead about this node arriving during a
                        // relay probe was never refuted and could stick until the
                        // reap timeout.
                        let refutable = match update.state {
                            NodeState::Suspect | NodeState::Dead => true,
                            NodeState::Left => !self.left,
                            NodeState::Alive => false,
                        };
                        if update.node_id == self.node_id && refutable {
                            self.refute(update.incarnation);
                        }
                    }

                    if is_ack_from_target {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    }

    fn membership_incarnation_of(&self, node_id: &NodeId) -> u64 {
        self.membership
            .get(node_id)
            .map(|m| m.incarnation)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mustard::transport::{InMemoryNetwork, InMemoryTransport};
    use std::time::Duration;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn fast_config() -> GossipConfig {
        GossipConfig {
            protocol_interval: Duration::from_millis(50),
            probe_timeout: Duration::from_millis(20),
            suspicion_timeout: Duration::from_millis(100),
            indirect_probe_count: 2,
            cleanup_timeout: Duration::from_millis(200),
            push_pull_interval: Duration::from_millis(500),
        }
    }

    #[tokio::test]
    async fn rejoin_contacts_are_bounded_and_retire_explicit_departures() {
        let net = InMemoryNetwork::new();
        let transport = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("root"), addr(1), fast_config(), transport);
        for number in 2..100 {
            node.handle_message(
                addr(number),
                GossipMessage::new(
                    NodeId::new(format!("peer-{number}")),
                    1,
                    GossipPayload::Ping { updates: vec![] },
                ),
            )
            .await;
        }
        assert_eq!(node.rejoin_contacts.len(), 16);
        for member in node
            .membership
            .iter()
            .filter(|member| member.node_id != node.node_id)
            .map(|member| MembershipUpdate {
                node_id: member.node_id.clone(),
                address: member.address,
                state: NodeState::Left,
                incarnation: member.incarnation,
            })
            .collect::<Vec<_>>()
        {
            node.membership.apply_update(&member, Instant::now());
        }
        node.config.cleanup_timeout = std::time::Duration::ZERO;
        node.run_one_cycle().await;
        assert!(
            node.rejoin_contacts.is_empty(),
            "explicit departures must not become fallback seeds"
        );
    }

    #[tokio::test]
    async fn reaped_contact_is_probed_even_with_another_live_peer() {
        let net = InMemoryNetwork::new();
        let transport = net.register(addr(1)).await;
        let returning = net.register(addr(3)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), transport);
        node.handle_message(
            addr(3),
            GossipMessage::new(
                NodeId::new("returning"),
                1,
                GossipPayload::Ping { updates: vec![] },
            ),
        )
        .await;
        returning.recv().await.unwrap(); // Consume the original acknowledgement.
        node.membership.declare_dead(&NodeId::new("returning"));
        node.membership.reap_dead();
        node.add_seed(NodeId::new("other"), addr(2));
        node.run_one_cycle().await;
        let probe =
            tokio::time::timeout(std::time::Duration::from_millis(100), returning.recv()).await;
        assert!(
            probe.is_ok(),
            "a live neighbour must not suppress rediscovery of a reaped peer"
        );
    }

    #[tokio::test]
    async fn seedless_bootstrap_rejoins_after_all_peers_are_reaped() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;
        let mut root = MustardNode::new(NodeId::new("root"), addr(1), fast_config(), t1);
        let mut peer = MustardNode::new(NodeId::new("peer"), addr(2), fast_config(), t2);
        // Bootstrap has no configured seeds, but has previously heard a real
        // peer. Both sides then lose contact long enough to declare death.
        root.handle_message(
            addr(2),
            GossipMessage::new(
                NodeId::new("peer"),
                1,
                GossipPayload::Ping { updates: vec![] },
            ),
        )
        .await;
        peer.add_seed(NodeId::new("root"), addr(1));
        peer.membership.declare_dead(&NodeId::new("root"));
        root.membership.declare_dead(&NodeId::new("peer"));
        root.membership.reap_dead();
        // Exhausting ordinary piggyback retransmissions must not make a Dead
        // claim impossible for the returning sender to discover and refute.
        for _ in 0..100 {
            peer.dissemination.select_updates();
        }
        let (root_tx, root_rx) = watch::channel(Vec::new());
        let (peer_tx, peer_rx) = watch::channel(Vec::new());
        root.set_membership_watch(root_tx);
        peer.set_membership_watch(peer_tx);
        let shutdown = CancellationToken::new();
        let root_shutdown = shutdown.clone();
        let peer_shutdown = shutdown.clone();
        let root_task = tokio::spawn(async move {
            root.run(root_shutdown).await;
            root
        });
        let peer_task = tokio::spawn(async move {
            peer.run(peer_shutdown).await;
            peer
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let converged =
            loop {
                let root_live = root_rx.borrow().iter().any(|node| {
                    node.node_id == NodeId::new("peer") && node.state == NodeState::Alive
                });
                let peer_live = peer_rx.borrow().iter().any(|node| {
                    node.node_id == NodeId::new("root") && node.state == NodeState::Alive
                });
                if root_live && peer_live {
                    break true;
                }
                if tokio::time::Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            };
        shutdown.cancel();
        root_task.await.unwrap();
        peer_task.await.unwrap();
        assert!(
            converged,
            "both nodes must rediscover each other before graceful shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ping_receives_ack() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);

        let (rejoin_tx, rejoin_rx) = watch::channel(false);
        node1.set_rejoin_watch(rejoin_tx);

        // n1 knows about n2
        node1.add_seed(NodeId::new("n2"), addr(2));

        assert!(
            !*rejoin_rx.borrow(),
            "seed membership is not rejoin evidence"
        );

        // n1 runs one probe cycle — should ping n2 and get ACK
        probe_answered_by(&mut node1, answer(&mut node2)).await;

        assert!(
            *rejoin_rx.borrow(),
            "a responding peer proves gossip rejoin"
        );

        // n2 should still be alive (not suspected)
        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Alive);
    }

    #[tokio::test]
    async fn unreachable_node_becomes_suspect() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        // Don't register addr(2) — n2 is unreachable

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let (rejoin_tx, rejoin_rx) = watch::channel(false);
        node1.set_rejoin_watch(rejoin_tx);
        // Tell n1 about n2 (but n2 isn't actually there)
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now(),
        );

        // Run a probe cycle — PING to n2 will timeout, no relays available
        node1.run_one_cycle().await;

        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Suspect);
        assert!(
            !*rejoin_rx.borrow(),
            "unreachable membership cannot prove rejoin"
        );
    }

    #[tokio::test]
    async fn quiesced_node_forms_no_opinions_about_its_peers() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        // n2 is unreachable, exactly as every peer is while n1's node-kill
        // gate drops all of its datagrams.
        let gate = crate::smoker::node_fault::NodeTransportGate::new();
        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node1.set_node_gate(gate.clone());
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now(),
        );

        gate.quiesce();
        for _ in 0..3 {
            node1.run_one_cycle().await;
            tokio::time::sleep(fast_config().suspicion_timeout).await;
        }

        // A killed node runs no failure detector. Suspicions formed while it
        // was cut off would spread after the gate reopens and knock healthy
        // peers out of everyone's live membership.
        let n2 = node1.membership.get(&NodeId::new("n2")).unwrap();
        assert_eq!(n2.state, NodeState::Alive);
        assert!(
            node1
                .dissemination
                .select_updates()
                .iter()
                .all(|update| update.node_id != NodeId::new("n2")),
            "no rumour about n2 may be queued while n1 is quiesced"
        );

        gate.restore();
        node1.run_one_cycle().await;
        assert_eq!(
            node1.membership.get(&NodeId::new("n2")).unwrap().state,
            NodeState::Suspect,
            "failure detection resumes once the gate reopens"
        );
    }

    #[tokio::test]
    async fn disseminated_suspect_carries_target_incarnation() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        // n2 unreachable.

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        // Add n2 with a distinctive incarnation of 5.
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            5,
            BTreeMap::new(),
            Instant::now(),
        );

        node1.run_one_cycle().await;

        // The disseminated Suspect update must carry n2's incarnation (5), not
        // the prober's — otherwise peers discard it or it overrides fresh state.
        let updates = node1.dissemination.select_updates();
        let suspect = updates
            .iter()
            .find(|u| u.node_id == NodeId::new("n2") && u.state == NodeState::Suspect)
            .expect("no Suspect update enqueued for n2");
        assert_eq!(suspect.incarnation, 5);
    }

    #[tokio::test]
    async fn membership_watch_publishes_state_transitions() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);

        let (tx, rx) = tokio::sync::watch::channel(Vec::new());
        node1.set_membership_watch(tx);

        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now(),
        );
        node1.publish_membership();
        assert!(
            rx.borrow()
                .iter()
                .any(|m| m.node_id == NodeId::new("n2") && m.state == NodeState::Alive)
        );

        // Suspect n2 — the active-member count is unchanged, but the watch must
        // still see the transition (the whole point of H7).
        node1.membership.suspect(&NodeId::new("n2"));
        node1.publish_membership();
        assert!(
            rx.borrow()
                .iter()
                .any(|m| m.node_id == NodeId::new("n2") && m.state == NodeState::Suspect),
            "state transition not published without a count change"
        );
    }

    #[tokio::test]
    async fn suspect_node_promoted_to_dead_after_timeout() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;

        let mut config = fast_config();
        config.suspicion_timeout = Duration::from_millis(50);

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), config, t1);

        // Add n2 and suspect it (suspect() sets state_changed to now).
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now(),
        );
        node1.membership.suspect(&NodeId::new("n2"));

        // Wait for the suspicion timeout (measured from state_changed) to elapse.
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Run a cycle — should promote n2 to Dead
        node1.run_one_cycle().await;

        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Dead);
    }

    #[tokio::test]
    async fn fresh_suspect_not_promoted_despite_stale_ack() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;

        let mut config = fast_config();
        config.suspicion_timeout = Duration::from_millis(50);
        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), config, t1);

        // n2's last ACK is ancient, but it was only *just* suspected. The
        // suspicion timer must run from suspicion start, not last_ack — so it
        // stays Suspect (gets its refutation window), not immediately Dead.
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now() - Duration::from_secs(10),
        );
        node1.membership.suspect(&NodeId::new("n2"));

        node1.run_one_cycle().await;

        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Suspect);
    }

    #[tokio::test]
    async fn suspect_refutation_bumps_incarnation() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);

        assert_eq!(node2.incarnation, 1);

        // Simulate receiving a gossip message that says we're suspected
        let suspect_msg = GossipMessage::new(
            NodeId::new("n1"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n2"),
                    address: addr(2),
                    state: NodeState::Suspect,
                    incarnation: 1,
                }],
            },
        );

        node2.handle_message(addr(1), suspect_msg).await;

        // Should have bumped incarnation to refute
        assert_eq!(node2.incarnation, 2);

        // The refutation Alive update was enqueued but then consumed
        // by the ACK reply (PING handler calls select_updates). The
        // important thing is that the incarnation was bumped — the
        // Alive update was already sent in the ACK.

        drop(t1); // suppress unused warning
    }

    #[tokio::test]
    async fn dead_refutation_bumps_incarnation() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        assert_eq!(node2.incarnation, 1);

        // A gossip message that falsely declares us Dead.
        let dead_msg = GossipMessage::new(
            NodeId::new("n1"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n2"),
                    address: addr(2),
                    state: NodeState::Dead,
                    incarnation: 1,
                }],
            },
        );

        node2.handle_message(addr(1), dead_msg).await;

        // We must refute a false Dead (not just Suspect) by bumping incarnation.
        assert_eq!(node2.incarnation, 2);

        drop(t1);
    }

    #[tokio::test]
    async fn piggybacked_updates_propagate_membership() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);

        // n1 knows about n3 but n2 doesn't
        node1.membership.add_node(
            NodeId::new("n3"),
            addr(3),
            1,
            BTreeMap::new(),
            Instant::now(),
        );
        node1.dissemination.enqueue(
            MembershipUpdate {
                node_id: NodeId::new("n3"),
                address: addr(3),
                state: NodeState::Alive,
                incarnation: 1,
            },
            3,
        );

        // n1 sends a PING to n2 with piggybacked n3 info
        node1.add_seed(NodeId::new("n2"), addr(2));

        let updates = node1.dissemination.select_updates();
        let ping = GossipMessage::new(NodeId::new("n1"), 1, GossipPayload::Ping { updates });
        node1.transport.send(addr(2), &ping).await.unwrap();

        // n2 receives and processes it
        let (from, msg) = node2.transport.recv().await.unwrap();
        node2.handle_message(from, msg).await;

        // n2 should now know about n3
        assert!(node2.membership.get(&NodeId::new("n3")).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn ping_timeout_triggers_ping_req() {
        // 3 nodes: A, B, C. Partition A↔B. When A probes B, the direct
        // PING times out. A should then send a PingReq to C (the only
        // alive relay). C forwards a Ping to B on A's behalf.
        //
        // Why C must be a responsive node (spawned with `node_c.run()`):
        //
        // `pick_probe_target()` selects a random member each cycle. If A
        // probes C first and nobody is draining C's inbox, C never sends
        // an ACK. A's `wait_for_ack` times out, and A marks C as Suspect.
        // Later, when A probes B and needs a relay, `alive_members()`
        // excludes the now-Suspect C. No relays are available, so A never
        // sends a PingReq and the test fails. This happened ~50% of the
        // time in CI.
        //
        // By spawning C as a real running node, C always responds to
        // PINGs and stays Alive, so it's always available as a relay.
        //
        // We verify the PingReq path by checking B's inbox (not C's):
        // C already consumed and processed the PingReq, so it's gone
        // from C's transport. But as part of handling it, C sent a Ping
        // to B. Since A↔B is partitioned but C↔B is not, B's inbox
        // should contain a Ping with sender=C — proof that the full
        // relay path (A → PingReq → C → Ping → B) was exercised.
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let tb = net.register(addr(2)).await;
        let tc = net.register(addr(3)).await;

        let mut node_a = MustardNode::new(NodeId::new("a"), addr(1), fast_config(), ta);
        let mut node_c = MustardNode::new(NodeId::new("c"), addr(3), fast_config(), tc);
        node_a.add_seed(NodeId::new("b"), addr(2));
        node_a.add_seed(NodeId::new("c"), addr(3));
        node_c.add_seed(NodeId::new("a"), addr(1));
        node_c.add_seed(NodeId::new("b"), addr(2));

        // Partition A↔B so direct PING is dropped
        net.partition(addr(1), addr(2)).await;

        // Run cycles until A picks B and marks it Suspect, with C answering
        // A's pings and PingReqs on the same task.
        for _ in 0..20 {
            probe_answered_by(&mut node_a, answer(&mut node_c)).await;
            if node_a
                .membership
                .get(&NodeId::new("b"))
                .is_some_and(|m| m.state == NodeState::Suspect)
            {
                break;
            }
        }

        // A should have marked B as Suspect
        assert_eq!(
            node_a.membership.get(&NodeId::new("b")).unwrap().state,
            NodeState::Suspect,
        );

        // C processed the PingReq by sending a Ping to B on A's behalf.
        // Since B is partitioned from A (but not from C), B's inbox should
        // contain at least one Ping from C — proof that the PingReq path
        // was exercised.
        let mut saw_forwarded_ping = false;
        while let Some((_from, msg)) = tb.try_recv() {
            if matches!(&msg.payload, GossipPayload::Ping { .. }) && msg.sender == NodeId::new("c")
            {
                saw_forwarded_ping = true;
            }
        }
        assert!(
            saw_forwarded_ping,
            "B should have received a Ping from C (relayed PingReq)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ping_req_relay_forwards_to_target_and_requester() {
        // When C receives a PingReq from A asking to probe B, C should:
        // 1. Send a Ping to B
        // 2. If B responds with Ack, forward an Ack to A (with sender=B)
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let tb = net.register(addr(2)).await;
        let tc = net.register(addr(3)).await;

        let mut node_b = MustardNode::new(NodeId::new("b"), addr(2), fast_config(), tb);
        let mut node_c = MustardNode::new(NodeId::new("c"), addr(3), fast_config(), tc);
        node_b.add_seed(NodeId::new("c"), addr(3));
        node_c.add_seed(NodeId::new("a"), addr(1));
        node_c.add_seed(NodeId::new("b"), addr(2));

        // A sends a PingReq to C
        let ping_req = GossipMessage::new(
            NodeId::new("a"),
            1,
            GossipPayload::PingReq {
                target: NodeId::new("b"),
                requester: NodeId::new("a"),
                updates: vec![],
            },
        );
        ta.send(addr(3), &ping_req).await.unwrap();

        // C handles the PingReq (will send Ping to B, wait for ACK, forward
        // to A) while B answers on the same task.
        let (from, msg) = node_c.transport.recv().await.unwrap();
        tokio::select! {
            biased;
            () = answer(&mut node_b) => {}
            () = node_c.handle_message(from, msg) => {}
        }

        // A should have received a forwarded ACK with sender=B
        let mut saw_forwarded_ack = false;
        while let Some((_from, msg)) = ta.try_recv() {
            if matches!(&msg.payload, GossipPayload::Ack { .. }) && msg.sender == NodeId::new("b") {
                saw_forwarded_ack = true;
            }
        }
        assert!(
            saw_forwarded_ack,
            "A should have received a forwarded ACK with sender=B"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn indirect_probe_success_prevents_suspect() {
        // A↔B partitioned, but A↔C and B↔C are fine. When A probes B,
        // the direct PING fails, but C relays successfully. B should
        // remain Alive in A's membership.
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let tb = net.register(addr(2)).await;
        let tc = net.register(addr(3)).await;

        let mut node_b = MustardNode::new(NodeId::new("b"), addr(2), fast_config(), tb);
        let mut node_c = MustardNode::new(NodeId::new("c"), addr(3), fast_config(), tc);
        node_b.add_seed(NodeId::new("c"), addr(3));
        node_c.add_seed(NodeId::new("a"), addr(1));
        node_c.add_seed(NodeId::new("b"), addr(2));

        // Partition A↔B only
        net.partition(addr(1), addr(2)).await;

        let mut node_a = MustardNode::new(NodeId::new("a"), addr(1), fast_config(), ta);
        node_a.add_seed(NodeId::new("b"), addr(2));
        node_a.add_seed(NodeId::new("c"), addr(3));

        // B and C answer while A runs its probe cycles. A's cycle will:
        // PING B (dropped), timeout, PingReq to C, C probes B (succeeds),
        // C forwards ACK to A, A receives it.
        //
        // Run cycles until A probes B. If A picks C, the cycle succeeds
        // normally. We keep going until A has probed B at least once.
        for _ in 0..20 {
            let peers = async {
                tokio::join!(answer(&mut node_b), answer(&mut node_c));
            };
            probe_answered_by(&mut node_a, peers).await;
        }

        // B should still be Alive (indirect probe via C saved it)
        let b_state = node_a.membership.get(&NodeId::new("b")).unwrap().state;
        assert_eq!(
            b_state,
            NodeState::Alive,
            "B should be Alive thanks to indirect probe via C, but was {b_state}"
        );
    }

    /// Answer everything that reaches `node` for as long as it is polled.
    async fn answer(node: &mut MustardNode<InMemoryTransport>) {
        while let Some((from, message)) = node.transport.recv().await {
            node.handle_message(from, message).await;
        }
    }

    /// Run one probe cycle on `prober` while `peers` answer on the same task.
    ///
    /// Spawned peer `run` loops let a loaded host starve them past the
    /// prober's 20 ms probe window. Here `biased` polls the peers first, so
    /// every message already delivered is answered before the prober looks
    /// again, and under paused time the clock only moves once both sides are
    /// idle.
    async fn probe_answered_by(
        prober: &mut MustardNode<InMemoryTransport>,
        peers: impl std::future::Future<Output = ()>,
    ) {
        tokio::select! {
            biased;
            () = peers => {}
            () = prober.run_one_cycle() => {}
        }
    }

    /// Probe `b` from `a` once, with `b` answering the PING after `delay`.
    /// Neither node has a relay for the other, like a three-node cluster
    /// that has already lost one member. Paused time and no spawned task
    /// make the ACK's lateness exact.
    async fn probe_without_relays(delay: Duration) -> NodeState {
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let tb = net.register(addr(2)).await;
        let mut a = MustardNode::new(NodeId::new("a"), addr(1), fast_config(), ta);
        let mut b = MustardNode::new(NodeId::new("b"), addr(2), fast_config(), tb);
        a.add_seed(NodeId::new("b"), addr(2));
        a.next_push_pull = Instant::now() + Duration::from_secs(3600);

        let b_answers = async {
            let (from, ping) = b.transport.recv().await.expect("a pings b");
            tokio::time::sleep(delay).await;
            b.handle_message(from, ping).await;
        };
        tokio::join!(a.run_one_cycle(), b_answers);
        a.membership.get(&NodeId::new("b")).unwrap().state
    }

    #[tokio::test(start_paused = true)]
    async fn late_ack_without_relays_keeps_the_target_alive() {
        let late = fast_config().probe_timeout * 3 / 2;
        assert_eq!(probe_without_relays(late).await, NodeState::Alive);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_past_both_probe_windows_without_relays_suspects_the_target() {
        let too_late = fast_config().probe_timeout * 5 / 2;
        assert_eq!(probe_without_relays(too_late).await, NodeState::Suspect);
    }

    /// Rounds between anti-entropy exchanges in the manual simulations: the
    /// default 10 s `push_pull_interval` over the default 500 ms probe period.
    const PUSH_PULL_EVERY_ROUNDS: usize = 20;

    #[tokio::test]
    async fn gossip_convergence_five_nodes() {
        // 5 nodes in a ring topology (each knows the next). After enough
        // rounds, every node should know about every other.
        //
        // We manually drive PING/ACK exchanges rather than spawning
        // concurrent tasks (tokio::spawn + start_paused is unreliable
        // under parallel test load; see tokio #3709), and use try_recv()
        // to drain messages without timers.
        //
        // Before anti-entropy, 73 schedules in 100,000 stranded a member for
        // good: every update about it spent its bounded re-broadcasts before
        // reaching some node, the queues drained, and no later round helped.
        // With push-pull all 100,000 converged by round 23, three rounds
        // after the first exchange. So this runs 2,000 fresh schedules every
        // time, from an OS-random base, and allows two exchanges' worth of
        // rounds; a failure prints the seed that replays it.
        let base: u64 = rand::random();
        for offset in 0..2_000u64 {
            let seed = base.wrapping_add(offset);
            let rounds = converge_five_node_ring(seed, Some(PUSH_PULL_EVERY_ROUNDS)).await;
            assert!(
                rounds.is_some_and(|round| round < 2 * PUSH_PULL_EVERY_ROUNDS),
                "seed {seed}: five-node ring took {rounds:?} rounds to converge"
            );
        }
    }

    #[tokio::test]
    async fn a_stranding_schedule_is_rescued_by_push_pull() {
        // Seed 830 is the first of the 73 schedules in 0..100,000 that
        // stranded a member permanently with piggybacking alone. The
        // simulation leaves the first exchange until round 20, so up to then
        // this replays the stranding schedule exactly, and push-pull has to
        // do the rescue.
        assert_eq!(
            converge_five_node_ring(830, None).await,
            None,
            "seed 830 no longer strands without anti-entropy; pick another stranding seed"
        );
        let rounds = converge_five_node_ring(830, Some(PUSH_PULL_EVERY_ROUNDS)).await;
        assert!(
            rounds.is_some_and(|round| round >= PUSH_PULL_EVERY_ROUNDS),
            "push-pull did not rescue the stranded member: {rounds:?}"
        );
    }

    /// Drive a seeded five-node ring until every node sees five active
    /// members. Returns the round it converged in, or `None` after 100.
    ///
    /// With `push_pull_every = Some(n)`, every node also starts an
    /// anti-entropy exchange on rounds n, 2n, 3n, … Round 0 is left to
    /// piggybacking so a schedule replays identically up to the first
    /// exchange, whichever way it is run.
    async fn converge_five_node_ring(seed: u64, push_pull_every: Option<usize>) -> Option<usize> {
        let net = InMemoryNetwork::new();
        let config = fast_config();

        let mut nodes = Vec::new();
        let mut addresses = Vec::new();

        for i in 0u16..5 {
            let a = addr(100 + i);
            addresses.push(a);
            let t = net.register(a).await;
            let mut node = MustardNode::new(NodeId::new(format!("n{i}")), a, config.clone(), t);
            node.seed_rng(seed.wrapping_mul(5).wrapping_add(u64::from(i)));
            nodes.push(node);
        }

        // Wire each node to know the next one (ring)
        for i in 0..nodes.len() {
            let next = (i + 1) % nodes.len();
            let id = NodeId::new(format!("n{next}"));
            let a = addresses[next];
            nodes[i].add_seed(id, a);
        }

        // Simulate gossip rounds. Each round:
        // 1. Every node picks a random peer and sends a PING (and, when an
        //    exchange is due, a push-pull request to another random peer)
        // 2. Every node drains its inbox (processing PINGs → sending
        //    ACKs, sync requests → sending its table, applying entries)
        // 3. Every node drains again (picking up the ACKs and sync replies)
        for round in 0..100 {
            let push_pull_due =
                push_pull_every.is_some_and(|every| round > 0 && round % every == 0);
            for node in &mut nodes {
                if let Some((_target_id, target_addr)) = node.pick_probe_target() {
                    let updates = node.dissemination.select_updates();
                    let ping = GossipMessage::new(
                        node.node_id.clone(),
                        node.incarnation,
                        GossipPayload::Ping { updates },
                    );
                    let _ = node.transport.send(target_addr, &ping).await;
                }
                if push_pull_due {
                    node.push_pull().await;
                }
            }

            for _ in 0..2 {
                for node in &mut nodes {
                    while let Some((from, msg)) = node.transport.try_recv() {
                        node.handle_message(from, msg).await;
                    }
                }
            }

            if nodes
                .iter()
                .all(|node| node.membership.active_members().len() == 5)
            {
                return Some(round);
            }
        }
        None
    }

    // -- anti-entropy push-pull (C6.5) ----------------------------------------

    fn entry(node: &str, port: u16, state: NodeState, incarnation: u64) -> MembershipUpdate {
        MembershipUpdate {
            node_id: NodeId::new(node),
            address: addr(port),
            state,
            incarnation,
        }
    }

    fn sync_from(sender: &str, entries: Vec<MembershipUpdate>, wants_reply: bool) -> GossipMessage {
        GossipMessage::new(
            NodeId::new(sender),
            1,
            GossipPayload::Sync {
                entries,
                wants_reply,
            },
        )
    }

    fn state_of<T: MustardTransport>(node: &MustardNode<T>, id: &str) -> Option<(NodeState, u64)> {
        node.membership
            .get(&NodeId::new(id))
            .map(|member| (member.state, member.incarnation))
    }

    /// Answer everything that reaches `node` for `duration`. Returns how many
    /// push-pull requests arrived.
    async fn serve(node: &mut MustardNode<InMemoryTransport>, duration: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + duration;
        let mut requests = 0;
        while tokio::time::Instant::now() < deadline {
            let Some((from, message)) = node.transport.try_recv() else {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            if matches!(
                message.payload,
                GossipPayload::Sync {
                    wants_reply: true,
                    ..
                }
            ) {
                requests += 1;
            }
            node.handle_message(from, message).await;
        }
        requests
    }

    #[tokio::test]
    async fn push_pull_cannot_resurrect_a_dead_node() {
        let net = InMemoryNetwork::new();
        let t = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), t);
        node.membership.add_node(
            NodeId::new("m"),
            addr(3),
            3,
            BTreeMap::new(),
            Instant::now(),
        );
        node.membership.declare_dead(&NodeId::new("m"));

        // A peer that never heard of the death still holds m Alive, at the
        // same or an older incarnation. Neither may bring m back.
        let stale = vec![
            entry("m", 3, NodeState::Alive, 3),
            entry("m", 3, NodeState::Alive, 2),
        ];
        node.handle_message(addr(2), sync_from("peer", stale, false))
            .await;

        assert_eq!(state_of(&node, "m"), Some((NodeState::Dead, 3)));
    }

    #[tokio::test]
    async fn push_pull_cannot_override_a_refutation() {
        let net = InMemoryNetwork::new();
        let t = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), t);
        // m refuted a suspicion by moving to incarnation 5.
        node.membership.add_node(
            NodeId::new("m"),
            addr(3),
            5,
            BTreeMap::new(),
            Instant::now(),
        );

        let stale = vec![
            entry("m", 3, NodeState::Suspect, 4),
            entry("m", 3, NodeState::Dead, 4),
        ];
        node.handle_message(addr(2), sync_from("peer", stale, false))
            .await;

        assert_eq!(state_of(&node, "m"), Some((NodeState::Alive, 5)));
    }

    #[tokio::test]
    async fn push_pull_does_not_introduce_a_member_we_only_hear_is_down() {
        let net = InMemoryNetwork::new();
        let t = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), t);

        let down = vec![
            entry("gone", 3, NodeState::Dead, 1),
            entry("left", 4, NodeState::Left, 1),
        ];
        node.handle_message(addr(2), sync_from("peer", down, false))
            .await;

        assert_eq!(state_of(&node, "gone"), None);
        assert_eq!(state_of(&node, "left"), None);
    }

    #[tokio::test]
    async fn push_pull_claim_about_ourselves_is_refuted() {
        let net = InMemoryNetwork::new();
        let t = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), t);

        let claim = vec![entry("observer", 1, NodeState::Suspect, 1)];
        node.handle_message(addr(2), sync_from("peer", claim, false))
            .await;

        assert_eq!(node.incarnation, 2);
        assert_eq!(state_of(&node, "observer"), Some((NodeState::Alive, 2)));
    }

    #[tokio::test]
    async fn push_pull_request_is_answered_once_with_the_whole_table() {
        let net = InMemoryNetwork::new();
        let t = net.register(addr(1)).await;
        let requester = net.register(addr(2)).await;
        let mut node = MustardNode::new(NodeId::new("observer"), addr(1), fast_config(), t);
        // Twenty others plus the observer and requester: three datagrams.
        for i in 0..20u16 {
            node.membership.add_node(
                NodeId::new(format!("m{i}")),
                addr(10 + i),
                1,
                BTreeMap::new(),
                Instant::now(),
            );
        }

        node.handle_message(addr(2), sync_from("requester", vec![], true))
            .await;

        let mut datagrams = 0;
        let mut seen = std::collections::HashSet::new();
        while let Some((_, reply)) = requester.try_recv() {
            let GossipPayload::Sync {
                entries,
                wants_reply,
            } = reply.payload
            else {
                panic!("expected only sync datagrams, got {:?}", reply.payload);
            };
            assert!(!wants_reply, "a reply must never ask for a reply");
            assert!(entries.len() <= MAX_PIGGYBACK_UPDATES);
            datagrams += 1;
            seen.extend(entries.into_iter().map(|entry| entry.node_id));
        }
        assert_eq!(datagrams, 3);
        assert_eq!(seen.len(), node.membership.len());

        // A reply is merged, never answered, or two nodes would bounce
        // their tables back and forth forever.
        node.handle_message(addr(2), sync_from("requester", vec![], false))
            .await;
        assert!(requester.try_recv().is_none());
    }

    #[tokio::test]
    async fn joining_node_learns_the_whole_cluster_on_its_first_cycle() {
        let net = InMemoryNetwork::new();
        let joiner_transport = net.register(addr(1)).await;
        let seed_transport = net.register(addr(2)).await;
        let mut seed =
            MustardNode::new(NodeId::new("seed"), addr(2), fast_config(), seed_transport);
        // The seed learnt these long ago: its dissemination queue has
        // nothing left to say about them, so only a full sync can teach
        // the joiner.
        for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
            seed.membership.add_node(
                NodeId::new(name),
                addr(10 + i as u16),
                1,
                BTreeMap::new(),
                Instant::now(),
            );
        }
        assert!(seed.dissemination.is_empty());

        let mut joiner = MustardNode::new(
            NodeId::new("joiner"),
            addr(1),
            fast_config(),
            joiner_transport,
        );
        joiner.add_seed(NodeId::new("seed"), addr(2));

        let ((), requests) = tokio::join!(
            joiner.run_one_cycle(),
            serve(&mut seed, Duration::from_millis(100))
        );
        assert_eq!(requests, 1, "the first cycle must start a push-pull");
        for name in ["seed", "a", "b", "c"] {
            assert_eq!(
                state_of(&joiner, name).map(|(state, _)| state),
                Some(NodeState::Alive),
                "joiner did not learn {name} from the join sync"
            );
        }
        assert!(state_of(&seed, "joiner").is_some());

        // The next exchange waits for `push_pull_interval`.
        let ((), requests) = tokio::join!(
            joiner.run_one_cycle(),
            serve(&mut seed, Duration::from_millis(100))
        );
        assert_eq!(requests, 0, "push-pull ran again before its interval");
    }

    // -- directory extension propagation (12b.2) ------------------------------

    #[tokio::test(start_paused = true)]
    async fn ack_carries_advertised_endpoints_to_the_prober() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node2.set_advertised_endpoints(9117, 9445, BTreeMap::new());
        node1.add_seed(NodeId::new("n2"), addr(2));

        // n1 probes n2; n2's ACK carries its directory extension.
        probe_answered_by(&mut node1, answer(&mut node2)).await;

        let endpoints = node1
            .directory()
            .endpoints
            .get(&NodeId::new("n2"))
            .expect("n1 should learn n2's endpoints from the ACK");
        assert_eq!(endpoints.api_address, addr(9117));
        assert_eq!(endpoints.reporting_address, addr(9445));
    }

    #[tokio::test]
    async fn leader_hint_relays_through_a_non_leader() {
        // n1 is the leader (local hint set); n2 learns the hint from n1's
        // PING and relays it to n3 — a node that never talks to the leader
        // directly still learns where it is. This is the H1 propagation
        // path for workers outside the council.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;
        let t3 = net.register(addr(3)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        let mut node3 = MustardNode::new(NodeId::new("n3"), addr(3), fast_config(), t3);
        node1.set_advertised_endpoints(9117, 9445, BTreeMap::new());
        node2.set_advertised_endpoints(9127, 9455, BTreeMap::new());
        node3.set_advertised_endpoints(9137, 9465, BTreeMap::new());

        let hint = LeaderHint {
            node_id: NodeId::new("n1"),
            term: 4,
            api_address: addr(9117),
            reporting_address: addr(9445),
        };
        let (hint_tx, hint_rx) = watch::channel(Some(hint.clone()));
        node1.set_leader_hint_watch(hint_rx);

        // n1 pings n2 directly.
        let ping = node1.stamp(GossipMessage::new(
            node1.node_id.clone(),
            node1.incarnation,
            GossipPayload::Ping { updates: vec![] },
        ));
        node1.transport.send(addr(2), &ping).await.unwrap();
        let (from, msg) = node2.transport.try_recv().unwrap();
        node2.handle_message(from, msg).await;
        assert_eq!(node2.directory().leader, Some(hint.clone()));

        // n2 (not a leader — no local hint) relays it to n3.
        let ping = node2.stamp(GossipMessage::new(
            node2.node_id.clone(),
            node2.incarnation,
            GossipPayload::Ping { updates: vec![] },
        ));
        node2.transport.send(addr(3), &ping).await.unwrap();
        let (from, msg) = node3.transport.try_recv().unwrap();
        node3.handle_message(from, msg).await;
        assert_eq!(node3.directory().leader, Some(hint));
        // n3 also learned n2's endpoints from the same datagram.
        assert!(node3.directory().endpoints.contains_key(&NodeId::new("n2")));

        drop(hint_tx);
    }

    #[tokio::test]
    async fn newer_leader_hint_overrides_the_old_one_after_failover() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("w"), addr(1), fast_config(), t1);

        let old = LeaderHint {
            node_id: NodeId::new("old-leader"),
            term: 3,
            api_address: addr(9117),
            reporting_address: addr(9445),
        };
        let new = LeaderHint {
            node_id: NodeId::new("new-leader"),
            term: 4,
            api_address: addr(9217),
            reporting_address: addr(9545),
        };

        let ext = |node_name: &str, hint: &LeaderHint| DirectoryExtension {
            node_id: NodeId::new(node_name),
            api_address: addr(1),
            reporting_address: addr(2),
            leader: Some(hint.clone()),
            labels: BTreeMap::new(),
            disk_pressured: false,
            hmac: [0u8; 32],
        };

        let mut msg = GossipMessage::new(
            NodeId::new("peer-a"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        msg.extension = Some(ext("peer-a", &old));
        node.handle_message(addr(2), msg).await;
        assert_eq!(node.directory().leader, Some(old.clone()));

        // The new leader's hint (higher term) wins; a stale replay loses.
        let mut msg = GossipMessage::new(
            NodeId::new("peer-b"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        msg.extension = Some(ext("peer-b", &new));
        node.handle_message(addr(3), msg).await;
        assert_eq!(node.directory().leader, Some(new.clone()));

        let mut msg = GossipMessage::new(
            NodeId::new("peer-c"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        msg.extension = Some(ext("peer-c", &old));
        node.handle_message(addr(4), msg).await;
        assert_eq!(node.directory().leader, Some(new));
    }

    #[tokio::test]
    async fn directory_watch_publishes_on_change() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let (tx, rx) = watch::channel(NodeDirectory::default());
        node.set_directory_watch(tx);
        node.set_advertised_endpoints(9117, 9445, BTreeMap::new());

        // Own endpoints published immediately.
        assert!(rx.borrow().endpoints.contains_key(&NodeId::new("n1")));

        let mut msg = GossipMessage::new(
            NodeId::new("n2"),
            1,
            GossipPayload::Ping { updates: vec![] },
        );
        msg.extension = Some(DirectoryExtension {
            node_id: NodeId::new("n2"),
            api_address: addr(9127),
            reporting_address: addr(9455),
            leader: None,
            labels: BTreeMap::new(),
            disk_pressured: false,
            hmac: [0u8; 32],
        });
        node.handle_message(addr(2), msg).await;
        assert!(rx.borrow().endpoints.contains_key(&NodeId::new("n2")));
    }

    #[tokio::test(start_paused = true)]
    async fn advertised_disk_pressure_reaches_a_remote_member_via_gossip() {
        // n2 advertises disk pressure; n1 probes it and must record n2 in its
        // directory's `disk_pressured` set — the wire path the leader's
        // reconciler reads to replace a pressured voter (12b.2 T3).
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node2.set_advertised_endpoints(9117, 9445, BTreeMap::new());
        let (pressure_tx, pressure_rx) = watch::channel(true);
        node2.set_disk_pressured_watch(pressure_rx);
        node1.add_seed(NodeId::new("n2"), addr(2));

        probe_answered_by(&mut node1, answer(&mut node2)).await;
        drop(pressure_tx);

        assert!(
            node1
                .directory()
                .disk_pressured
                .contains(&NodeId::new("n2")),
            "n1 must learn n2's advertised disk pressure from gossip"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn advertised_labels_reach_a_remote_member_via_gossip() {
        // n2 advertises a zone label; n1 probes it and must learn that
        // label on n2's membership record — the wire path that makes
        // label filtering and zone-aware council selection live (CP7).
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node2.set_advertised_endpoints(
            9117,
            9445,
            BTreeMap::from([("zone".to_string(), "us-east".to_string())]),
        );
        node1.add_seed(NodeId::new("n2"), addr(2));

        probe_answered_by(&mut node1, answer(&mut node2)).await;

        let member = node1
            .membership
            .get(&NodeId::new("n2"))
            .expect("n1 should know n2");
        assert_eq!(
            member.labels.get("zone").map(String::as_str),
            Some("us-east"),
            "n1 must learn n2's advertised zone label from gossip"
        );
        // The directory carries it too (what council selection reads).
        assert_eq!(
            node1
                .directory()
                .labels
                .get(&NodeId::new("n2"))
                .and_then(|l| l.get("zone"))
                .map(String::as_str),
            Some("us-east")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_relabelled_member_is_republished_without_a_state_change() {
        // V02 soak bug 5: a node restarted with new labels (same state,
        // same incarnation) and `relish nodes` kept its old ones for good,
        // because the snapshot digest ignored labels.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;
        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        let (tx, rx) = watch::channel(Vec::new());
        node1.set_membership_watch(tx);
        let zone = |labels: BTreeMap<String, String>| {
            BTreeMap::from([("zone".to_string(), labels["zone"].clone())])
        };
        node2.set_advertised_endpoints(
            9117,
            9445,
            zone(BTreeMap::from([("zone".into(), "us-east".into())])),
        );
        node1.add_seed(NodeId::new("n2"), addr(2));
        probe_answered_by(&mut node1, answer(&mut node2)).await;
        node1.publish_membership();

        node2.set_advertised_endpoints(
            9117,
            9445,
            zone(BTreeMap::from([("zone".into(), "us-west".into())])),
        );
        probe_answered_by(&mut node1, answer(&mut node2)).await;
        node1.publish_membership();

        let published = rx.borrow().clone();
        let n2 = published
            .iter()
            .find(|member| member.node_id == NodeId::new("n2"))
            .expect("n2 published");
        assert_eq!(
            n2.labels.get("zone").map(String::as_str),
            Some("us-west"),
            "the snapshot kept n2's old labels"
        );
    }

    // -- graceful leave -------------------------------------------------------

    #[tokio::test]
    async fn leave_broadcasts_left_state() {
        // When a node calls leave(), it should send PINGs containing
        // a Left update for itself to its peers.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node1.add_seed(NodeId::new("n2"), addr(2));

        node1.leave().await;

        // n1 should now be Left in its own table
        assert_eq!(
            node1.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Left,
        );

        // n2's inbox should contain a PING with a Left update for n1
        let mut saw_left_update = false;
        while let Some((_from, msg)) = t2.try_recv() {
            for update in msg.payload.updates() {
                if update.node_id == NodeId::new("n1") && update.state == NodeState::Left {
                    saw_left_update = true;
                }
            }
        }
        assert!(
            saw_left_update,
            "n2 should have received a Left update for n1"
        );
    }

    #[tokio::test]
    async fn other_node_applies_left_update() {
        // When n2 receives n1's Left update, it should mark n1 as Left.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node1.add_seed(NodeId::new("n2"), addr(2));
        node2.add_seed(NodeId::new("n1"), addr(1));

        node1.leave().await;

        // n2 handles the incoming message
        while let Some((from, msg)) = node2.transport.try_recv() {
            node2.handle_message(from, msg).await;
        }

        assert_eq!(
            node2.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Left,
        );
    }

    #[tokio::test]
    async fn left_node_not_selected_as_probe_target() {
        // A node in the Left state should not be picked for probing.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let _t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node1.add_seed(NodeId::new("n2"), addr(2));

        // Mark n2 as Left
        node1.membership.get_mut(&NodeId::new("n2")).unwrap().state = NodeState::Left;

        // n1 should have no probe targets (only itself is alive)
        assert!(node1.pick_probe_target().is_none());
    }

    #[tokio::test]
    async fn graceful_shutdown_sends_leave() {
        // When run() exits via CancellationToken, it should call leave()
        // and the peer should receive a Left update.
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node1.add_seed(NodeId::new("n2"), addr(2));
        node2.add_seed(NodeId::new("n1"), addr(1));

        // Run n1 briefly then cancel
        let shutdown = CancellationToken::new();
        let shutdown1 = shutdown.clone();
        shutdown.cancel();
        node1.run(shutdown1).await;

        // n2 handles whatever n1 sent during shutdown
        while let Some((from, msg)) = node2.transport.try_recv() {
            node2.handle_message(from, msg).await;
        }

        assert_eq!(
            node2.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Left,
        );
    }

    /// M12: a node that restarts at incarnation 1 while peers hold it Dead at a
    /// far higher incarnation refutes past that value in a single step, so its
    /// Alive immediately out-ranks the stale Dead instead of needing dozens of
    /// refutes.
    #[tokio::test]
    async fn refute_seeds_incarnation_past_a_stale_dead() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        assert_eq!(node.incarnation, 1);

        // A peer gossips that we are Dead at incarnation 50 (we crashed there
        // and restarted at 1).
        let stale_dead = GossipMessage::new(
            NodeId::new("peer"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n1"),
                    address: addr(1),
                    state: NodeState::Dead,
                    incarnation: 50,
                }],
            },
        );
        node.handle_message(addr(9), stale_dead).await;

        assert_eq!(
            node.incarnation, 51,
            "refute must jump past the stale Dead's incarnation in one step"
        );
    }

    /// O9: refuting told the cluster we're alive but left our own record
    /// holding the claim we'd just refuted, so the node disagreed with
    /// everyone else about itself — including the scheduler and council reads
    /// that go through the membership table.
    #[tokio::test]
    async fn refute_marks_this_node_alive_in_its_own_table() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node.membership.add_node(
            NodeId::new("n1"),
            addr(1),
            1,
            BTreeMap::new(),
            Instant::now(),
        );

        let suspected = GossipMessage::new(
            NodeId::new("peer"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n1"),
                    address: addr(1),
                    state: NodeState::Suspect,
                    incarnation: 4,
                }],
            },
        );
        node.handle_message(addr(9), suspected).await;

        assert_eq!(
            node.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Alive,
            "a node that refuted suspicion still saw itself as Suspect"
        );
    }

    /// O9: `Left` was the one claim a node could not refute about itself, so a
    /// replayed departure (gossip is authenticated, so replayable but not
    /// forgeable) took a healthy node out until the 60s reap.
    #[tokio::test]
    async fn a_replayed_left_does_not_evict_a_running_node() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node.membership.add_node(
            NodeId::new("n1"),
            addr(1),
            1,
            BTreeMap::new(),
            Instant::now(),
        );

        let replayed_left = GossipMessage::new(
            NodeId::new("peer"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n1"),
                    address: addr(1),
                    state: NodeState::Left,
                    incarnation: 1,
                }],
            },
        );
        node.handle_message(addr(9), replayed_left).await;

        assert_eq!(
            node.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Alive,
            "a replayed Left evicted a node that never left"
        );
        assert!(
            node.incarnation > 1,
            "refuting a Left must bump the incarnation so peers prefer it"
        );
    }

    /// …but a node that really did leave must stay left. Its own departure
    /// echoing back off a peer is not a replay to refute.
    #[tokio::test]
    async fn a_departed_node_does_not_resurrect_itself_on_its_own_echo() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let mut node = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        node.membership.add_node(
            NodeId::new("n1"),
            addr(1),
            1,
            BTreeMap::new(),
            Instant::now(),
        );
        node.leave().await;

        let echoed = GossipMessage::new(
            NodeId::new("peer"),
            1,
            GossipPayload::Ping {
                updates: vec![MembershipUpdate {
                    node_id: NodeId::new("n1"),
                    address: addr(1),
                    state: NodeState::Left,
                    incarnation: node.incarnation,
                }],
            },
        );
        node.handle_message(addr(9), echoed).await;

        assert_eq!(
            node.membership.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Left,
            "a deliberate shutdown was undone by its own gossip echo"
        );
    }

    /// M13: a relayed ACK (forwarded by a relay on the target's behalf) must
    /// not overwrite the target's recorded address with the relay's socket —
    /// doing so made the next direct probe hit the relay and falsely evict the
    /// healthy target.
    #[tokio::test]
    async fn relayed_ack_does_not_rewrite_the_targets_address() {
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let mut node_a = MustardNode::new(NodeId::new("a"), addr(1), fast_config(), ta);
        // A knows B at its real address (addr 2).
        node_a.membership.add_node(
            NodeId::new("b"),
            addr(2),
            1,
            BTreeMap::new(),
            Instant::now(),
        );

        // A relay C (socket addr 3) forwards B's ACK to A.
        let relayed = GossipMessage::new(
            NodeId::new("b"),
            2,
            GossipPayload::Ack {
                updates: vec![],
                relayed: true,
            },
        );
        node_a.handle_message(addr(3), relayed).await;

        // B's address is unchanged — still its own, not the relay's.
        assert_eq!(
            node_a.membership.get(&NodeId::new("b")).unwrap().address,
            addr(2),
            "a relayed ACK must not record the target at the relay's address"
        );

        // A direct ACK, by contrast, does refresh the address (B legitimately
        // reachable at the socket it came from).
        let direct = GossipMessage::new(
            NodeId::new("b"),
            2,
            GossipPayload::Ack {
                updates: vec![],
                relayed: false,
            },
        );
        node_a.handle_message(addr(2), direct).await;
        assert_eq!(
            node_a.membership.get(&NodeId::new("b")).unwrap().address,
            addr(2),
        );
    }
}

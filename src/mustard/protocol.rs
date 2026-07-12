/// SWIM probe cycle protocol.
///
/// Each protocol period, the node:
/// 1. Picks a random alive peer to probe.
/// 2. Sends a PING (with piggybacked membership updates).
/// 3. Waits for an ACK within `probe_timeout`.
/// 4. If no ACK, sends PING-REQ to `indirect_probe_count` random peers.
/// 5. If still no ACK, marks the target as Suspect.
/// 6. Promotes expired suspects to Dead.
///
/// The `MustardNode` struct owns the membership table, dissemination
/// queue, and transport, and drives the protocol as an async task.
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use rand::seq::SliceRandom;
use tokio_util::sync::CancellationToken;

use tokio::sync::watch;

use crate::meat::NodeId;

use super::config::GossipConfig;
use super::directory::NodeDirectory;
use super::dissemination::DisseminationQueue;
use super::membership::{MembershipSnapshot, MembershipTable};
use super::message::{
    DirectoryExtension, GossipMessage, GossipPayload, LeaderHint, MembershipUpdate,
};
use super::state::NodeState;
use super::transport::MustardTransport;

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
    /// Lamport clock for causal ordering.
    lamport: u64,
    /// Optional watch channel for publishing membership snapshots.
    /// Set when running inside the agent, None in standalone tests.
    membership_watch: Option<watch::Sender<Vec<MembershipSnapshot>>>,
    /// Digest of the last-published membership. Used to publish on any content
    /// change (state/incarnation/council/leader), not just a count change.
    last_published_digest: Vec<(NodeId, NodeState, u64, bool, bool)>,
    /// Seed addresses to (re-)contact while this node has no live peers.
    /// Used to bootstrap a join by address without inserting a placeholder
    /// member: we ping the seed, and learn its real identity from the reply.
    seeds: Vec<SocketAddr>,
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
    /// Directory accumulated from received extensions.
    directory: NodeDirectory,
    /// Optional watch channel publishing the directory on change.
    directory_watch: Option<watch::Sender<NodeDirectory>>,
}

impl<T: MustardTransport> MustardNode<T> {
    /// Maximum number of peers to notify during graceful leave.
    const MAX_LEAVE_FANOUT: usize = 10;

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
            lamport: 0,
            membership_watch: None,
            last_published_digest: Vec::new(),
            seeds: Vec::new(),
            advertised: None,
            advertised_labels: BTreeMap::new(),
            leader_hint_rx: None,
            disk_pressured_rx: None,
            directory: NodeDirectory::default(),
            directory_watch: None,
        }
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

    /// Ping every seed address with an empty Ping. The reply registers the
    /// seed by its real identity and carries piggybacked membership, so one
    /// successful round bootstraps the whole view; lost UDP datagrams are
    /// retried on the next probe cycle while still isolated.
    async fn ping_seeds(&self) {
        for &addr in &self.seeds {
            let ping = GossipMessage::new(
                self.node_id.clone(),
                self.incarnation,
                GossipPayload::Ping {
                    updates: Vec::new(),
                },
            );
            let _ = self.transport.send(addr, &self.stamp(ping)).await;
        }
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
        let snapshot = self.membership.snapshot();
        let digest: Vec<(NodeId, NodeState, u64, bool, bool)> = snapshot
            .iter()
            .map(|m| {
                (
                    m.node_id.clone(),
                    m.state,
                    m.incarnation,
                    m.is_council,
                    m.is_leader,
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

        // Mark ourselves as Left
        if let Some(member) = self.membership.get_mut(&self.node_id) {
            member.state = NodeState::Left;
            member.state_changed = now;
        }

        // Enqueue Left update for dissemination
        self.tick_lamport();
        self.dissemination.enqueue(
            MembershipUpdate {
                node_id: self.node_id.clone(),
                address: self.address,
                state: NodeState::Left,
                incarnation: self.incarnation,
                lamport: self.lamport,
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
    /// On shutdown, announces graceful departure via [`leave()`] before
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
        self.promote_expired_suspects();
        let now = Instant::now();
        let reaped = self
            .membership
            .reap_expired_dead(self.config.cleanup_timeout, now);
        if !reaped.is_empty() && self.directory.prune(&reaped) {
            self.publish_directory();
        }

        let target = self.pick_probe_target();
        let Some((target_id, target_addr)) = target else {
            // No live peers — we're isolated. If we have seed addresses,
            // ping them directly to (re)join. Their replies register them
            // by their real NodeId via handle_message, so no placeholder
            // member is ever created.
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

        // Wait for indirect ACK
        if !relays.is_empty() {
            let got_indirect = self
                .wait_for_ack(&target_id, self.config.probe_timeout)
                .await;
            if got_indirect {
                return;
            }
        }

        // No ACK at all — mark as suspect
        if self.membership.suspect(&target_id) {
            self.tick_lamport();
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
                    lamport: self.lamport,
                },
                self.membership.len(),
            );
        }
    }

    /// Handle an incoming gossip message.
    pub async fn handle_message(&mut self, from: SocketAddr, message: GossipMessage) {
        let now = Instant::now();

        // Fold the directory extension in first — endpoint knowledge and the
        // leader hint are useful even if the membership payload is stale.
        if let Some(extension) = &message.extension {
            self.ingest_extension(extension);
        }

        // Register the sender if we haven't seen them
        let is_new = self.membership.add_node(
            message.sender.clone(),
            from,
            message.incarnation,
            BTreeMap::new(),
            now,
        );

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
            self.tick_lamport();
            self.dissemination.enqueue(
                MembershipUpdate {
                    node_id: message.sender.clone(),
                    address: from,
                    state: NodeState::Alive,
                    incarnation: message.incarnation,
                    lamport: self.lamport,
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

            // If we're being suspected *or* declared dead, refute it. Refuting
            // Dead matters as much as Suspect: without it a false Dead about us
            // is unrecoverable until the 60s reap (we'd be invisible to
            // scheduling and the council the whole time). A higher incarnation
            // resurrects us — see `resolve_conflict`.
            if update.node_id == self.node_id
                && matches!(update.state, NodeState::Suspect | NodeState::Dead)
            {
                self.refute();
            }
        }

        // Handle the message type
        match &message.payload {
            GossipPayload::Ping { .. } => {
                // Reply with ACK
                let updates = self.dissemination.select_updates();
                let ack = GossipMessage::new(
                    self.node_id.clone(),
                    self.incarnation,
                    GossipPayload::Ack { updates },
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
                                },
                            );
                            let _ = self.transport.send(req_addr, &self.stamp(fwd_ack)).await;
                        }
                    }
                }
            }
            GossipPayload::Ack { .. } => {
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
    fn refute(&mut self) {
        self.incarnation += 1;
        self.tick_lamport();
        self.dissemination.enqueue(
            MembershipUpdate {
                node_id: self.node_id.clone(),
                address: self.address,
                state: NodeState::Alive,
                incarnation: self.incarnation,
                lamport: self.lamport,
            },
            self.membership.len(),
        );
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
                self.tick_lamport();
                let inc = self.membership_incarnation_of(&node_id);
                self.dissemination.enqueue(
                    MembershipUpdate {
                        node_id,
                        address: node_addr,
                        state: NodeState::Dead,
                        incarnation: inc,
                        lamport: self.lamport,
                    },
                    self.membership.len(),
                );
            }
        }
    }

    /// Pick a random alive peer to probe (not ourselves).
    pub fn pick_probe_target(&self) -> Option<(NodeId, SocketAddr)> {
        let candidates: Vec<_> = self
            .membership
            .active_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id)
            .collect();

        if candidates.is_empty() {
            return None;
        }

        let mut rng = rand::thread_rng();
        let target = candidates.choose(&mut rng).unwrap();
        Some((target.node_id.clone(), target.address))
    }

    /// Pick random relay nodes for indirect probing (not ourselves, not the target).
    fn pick_relays(&self, target: &NodeId) -> Vec<SocketAddr> {
        let candidates: Vec<_> = self
            .membership
            .alive_members()
            .into_iter()
            .filter(|m| m.node_id != self.node_id && m.node_id != *target)
            .collect();

        let mut rng = rand::thread_rng();
        let count = self.config.indirect_probe_count.min(candidates.len());
        candidates
            .choose_multiple(&mut rng, count)
            .map(|m| m.address)
            .collect()
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

    fn tick_lamport(&mut self) {
        self.lamport += 1;
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
    use crate::mustard::transport::InMemoryNetwork;
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
        }
    }

    #[tokio::test]
    async fn ping_receives_ack() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);

        // n1 knows about n2
        node1.add_seed(NodeId::new("n2"), addr(2));

        // Spawn n2 to handle incoming messages
        let shutdown = CancellationToken::new();
        let shutdown2 = shutdown.clone();
        let handle = tokio::spawn(async move {
            node2.run(shutdown2).await;
            node2
        });

        // n1 runs one probe cycle — should ping n2 and get ACK
        node1.run_one_cycle().await;

        // n2 should still be alive (not suspected)
        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Alive);

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn unreachable_node_becomes_suspect() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        // Don't register addr(2) — n2 is unreachable

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
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
                    lamport: 1,
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
                    lamport: 1,
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
                lamport: 1,
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

    #[tokio::test]
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

        // Spawn C so it responds to A's pings and PingReqs
        let shutdown = CancellationToken::new();
        let shutdown_c = shutdown.clone();
        let handle_c = tokio::spawn(async move {
            node_c.run(shutdown_c).await;
            node_c
        });

        // Run cycles until A picks B and marks it Suspect
        for _ in 0..20 {
            node_a.run_one_cycle().await;
            if node_a
                .membership
                .get(&NodeId::new("b"))
                .is_some_and(|m| m.state == NodeState::Suspect)
            {
                break;
            }
        }

        shutdown.cancel();
        let _node_c = handle_c.await.unwrap();

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

    #[tokio::test]
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

        // C processes the PingReq — spawns a Ping to B and waits for ACK.
        // We need B to respond, so spawn B's handler concurrently.
        let shutdown = CancellationToken::new();
        let shutdown_b = shutdown.clone();
        let handle_b = tokio::spawn(async move {
            node_b.run(shutdown_b).await;
            node_b
        });

        // C handles the PingReq (will send Ping to B, wait for ACK, forward to A)
        let (from, msg) = node_c.transport.recv().await.unwrap();
        node_c.handle_message(from, msg).await;

        shutdown.cancel();
        let _node_b = handle_b.await.unwrap();

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

    #[tokio::test]
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

        // Spawn B and C so they can handle messages while A runs its probe cycle.
        // A's run_one_cycle will: PING B (dropped), timeout, PingReq to C,
        // C probes B (succeeds), C forwards ACK to A, A receives it.
        let shutdown = CancellationToken::new();
        let shutdown_b = shutdown.clone();
        let shutdown_c = shutdown.clone();
        let handle_b = tokio::spawn(async move {
            node_b.run(shutdown_b).await;
            node_b
        });
        let handle_c = tokio::spawn(async move {
            node_c.run(shutdown_c).await;
            node_c
        });

        // Run cycles until A probes B. If A picks C, the cycle succeeds
        // normally. We keep going until A has probed B at least once.
        for _ in 0..20 {
            node_a.run_one_cycle().await;
        }

        shutdown.cancel();
        let _ = handle_b.await;
        let _ = handle_c.await;

        // B should still be Alive (indirect probe via C saved it)
        let b_state = node_a.membership.get(&NodeId::new("b")).unwrap().state;
        assert_eq!(
            b_state,
            NodeState::Alive,
            "B should be Alive thanks to indirect probe via C, but was {b_state}"
        );
    }

    #[tokio::test]
    async fn gossip_convergence_five_nodes() {
        // 5 nodes in a ring topology (each knows the next).
        // After enough cycles, all nodes should know about all others.
        //
        // We manually drive PING/ACK exchanges rather than spawning
        // concurrent tasks (tokio::spawn + start_paused is unreliable
        // under parallel test load; see tokio #3709). And we use
        // try_recv() to drain messages without involving timers, so
        // the test is fully deterministic modulo random target selection.
        let net = InMemoryNetwork::new();
        let config = fast_config();

        let mut nodes = Vec::new();
        let mut addresses = Vec::new();

        for i in 0u16..5 {
            let a = addr(100 + i);
            addresses.push(a);
            let t = net.register(a).await;
            let node = MustardNode::new(NodeId::new(format!("n{i}")), a, config.clone(), t);
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
        // 1. Every node picks a random peer and sends a PING
        // 2. Every node drains its inbox (processing PINGs → sending
        //    ACKs, applying piggybacked updates)
        // 3. Every node drains again (picking up the ACKs)
        for _ in 0..100 {
            // Phase 1: each node sends a PING to a random peer
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
            }

            // Phase 2+3: drain messages twice (PINGs then ACKs)
            for _ in 0..2 {
                for node in &mut nodes {
                    while let Some((from, msg)) = node.transport.try_recv() {
                        node.handle_message(from, msg).await;
                    }
                }
            }
        }

        // Every node should know about all 5 members
        for node in &nodes {
            let alive_count = node.membership.active_members().len();
            assert_eq!(
                alive_count, 5,
                "node {} sees {} active members, expected 5",
                node.node_id, alive_count
            );
        }
    }

    // -- directory extension propagation (12b.2) ------------------------------

    #[tokio::test]
    async fn ack_carries_advertised_endpoints_to_the_prober() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;
        let t2 = net.register(addr(2)).await;

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
        let mut node2 = MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);
        node2.set_advertised_endpoints(9117, 9445, BTreeMap::new());
        node1.add_seed(NodeId::new("n2"), addr(2));

        let shutdown = CancellationToken::new();
        let shutdown2 = shutdown.clone();
        let handle = tokio::spawn(async move {
            node2.run(shutdown2).await;
            node2
        });

        // n1 probes n2; n2's ACK carries its directory extension.
        node1.run_one_cycle().await;
        shutdown.cancel();
        let _ = handle.await;

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

    #[tokio::test]
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

        let shutdown = CancellationToken::new();
        let shutdown2 = shutdown.clone();
        let handle = tokio::spawn(async move {
            node2.run(shutdown2).await;
            node2
        });

        node1.run_one_cycle().await;
        shutdown.cancel();
        let _ = handle.await;
        drop(pressure_tx);

        assert!(
            node1
                .directory()
                .disk_pressured
                .contains(&NodeId::new("n2")),
            "n1 must learn n2's advertised disk pressure from gossip"
        );
    }

    #[tokio::test]
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

        let shutdown = CancellationToken::new();
        let shutdown2 = shutdown.clone();
        let handle = tokio::spawn(async move {
            node2.run(shutdown2).await;
            node2
        });

        node1.run_one_cycle().await;
        shutdown.cancel();
        let _ = handle.await;

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
}

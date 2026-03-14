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

use crate::patty::NodeId;

use super::config::GossipConfig;
use super::dissemination::DisseminationQueue;
use super::membership::MembershipTable;
use super::message::{GossipMessage, GossipPayload, MembershipUpdate};
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
}

impl<T: MustardTransport> MustardNode<T> {
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
        }
    }

    /// Add a seed node to bootstrap cluster discovery.
    pub fn add_seed(&mut self, node_id: NodeId, address: SocketAddr) {
        self.membership
            .add_node(node_id, address, 1, BTreeMap::new(), Instant::now());
    }

    /// Run the protocol loop until cancelled.
    pub async fn run(&mut self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(self.config.protocol_interval);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    self.run_one_cycle().await;
                }
                msg = self.transport.recv() => {
                    if let Some((from, message)) = msg {
                        self.handle_message(from, message).await;
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

        let target = self.pick_probe_target();
        let Some((target_id, target_addr)) = target else {
            return; // No peers to probe
        };

        // Send PING
        let updates = self.dissemination.select_updates();
        let ping = GossipMessage::new(
            self.node_id.clone(),
            self.incarnation,
            GossipPayload::Ping { updates },
        );
        let _ = self.transport.send(target_addr, &ping).await;

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
            let _ = self.transport.send(*relay_addr, &ping_req).await;
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
                    node_id: target_id,
                    address: target_addr,
                    state: NodeState::Suspect,
                    incarnation: self.membership_incarnation_of(&ping.sender),
                    lamport: self.lamport,
                },
                self.membership.len(),
            );
        }
    }

    /// Handle an incoming gossip message.
    pub async fn handle_message(&mut self, from: SocketAddr, message: GossipMessage) {
        let now = Instant::now();

        // Register the sender if we haven't seen them
        let is_new = self.membership.add_node(
            message.sender.clone(),
            from,
            message.incarnation,
            BTreeMap::new(),
            now,
        );

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

            // If we're being suspected, refute it
            if update.node_id == self.node_id && update.state == NodeState::Suspect {
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
                let _ = self.transport.send(from, &ack).await;
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
                    let _ = self.transport.send(target_addr, &ping).await;

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
                            let _ = self.transport.send(req_addr, &fwd_ack).await;
                        }
                    }
                }
            }
            GossipPayload::Ack { .. } => {
                // Mark sender as alive (ACK received)
                if let Some(member) = self.membership.get_mut(&message.sender) {
                    if member.state == NodeState::Suspect {
                        member.state = NodeState::Alive;
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
            if member.state == NodeState::Suspect
                && member.node_id != self.node_id
                && now.duration_since(member.last_ack) > timeout
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
    async fn suspect_node_promoted_to_dead_after_timeout() {
        let net = InMemoryNetwork::new();
        let t1 = net.register(addr(1)).await;

        let mut config = fast_config();
        config.suspicion_timeout = Duration::from_millis(50);

        let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), config, t1);

        // Add n2 and immediately suspect it
        node1.membership.add_node(
            NodeId::new("n2"),
            addr(2),
            1,
            BTreeMap::new(),
            // last_ack far in the past
            Instant::now() - Duration::from_secs(10),
        );
        node1.membership.suspect(&NodeId::new("n2"));

        // Wait for suspicion timeout to elapse
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Run a cycle — should promote n2 to Dead
        node1.run_one_cycle().await;

        let n2_state = node1.membership.get(&NodeId::new("n2")).unwrap().state;
        assert_eq!(n2_state, NodeState::Dead);
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
        // PING times out. A should then send a PingReq to C (the only relay).
        let net = InMemoryNetwork::new();
        let ta = net.register(addr(1)).await;
        let _tb = net.register(addr(2)).await;
        let tc = net.register(addr(3)).await;

        let mut node_a = MustardNode::new(NodeId::new("a"), addr(1), fast_config(), ta);
        node_a.add_seed(NodeId::new("b"), addr(2));
        node_a.add_seed(NodeId::new("c"), addr(3));

        // Partition A↔B so direct PING is dropped
        net.partition(addr(1), addr(2)).await;

        // Run cycles until A picks B as the probe target. If A picks C
        // (reachable), the cycle completes normally — try again.
        // We know A picked B if B becomes Suspect afterwards (since the
        // PingReq relay also can't help — C isn't running to handle it).
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

        // A should have marked B as Suspect (after failed direct + indirect probe)
        assert_eq!(
            node_a.membership.get(&NodeId::new("b")).unwrap().state,
            NodeState::Suspect,
        );

        // C should have received at least one PingReq asking to probe B
        let mut saw_ping_req = false;
        while let Some((_from, msg)) = tc.try_recv() {
            if let GossipPayload::PingReq {
                target, requester, ..
            } = &msg.payload
            {
                assert_eq!(*target, NodeId::new("b"));
                assert_eq!(*requester, NodeId::new("a"));
                saw_ping_req = true;
            }
        }
        assert!(saw_ping_req, "C should have received a PingReq for B");
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
}

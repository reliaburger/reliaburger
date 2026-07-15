//! Deterministic in-memory gossip simulation shared by benchmarks and the
//! deterministic correctness tests.

use std::collections::HashMap;
use std::net::SocketAddr;

use reliaburger::meat::NodeId;
use reliaburger::mustard::{
    GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork, InMemoryTransport, MustardNode,
    MustardTransport,
};

fn node_addr(index: usize) -> SocketAddr {
    let port = (index % 60_000) as u16 + 1_024;
    let third_octet = ((index / 60_000) & 0xff) as u8;
    SocketAddr::from(([10, 0, third_octet, 1], port))
}

fn fast_config() -> GossipConfig {
    GossipConfig {
        protocol_interval: std::time::Duration::from_millis(50),
        probe_timeout: std::time::Duration::from_millis(20),
        suspicion_timeout: std::time::Duration::from_millis(100),
        indirect_probe_count: 2,
        cleanup_timeout: std::time::Duration::from_secs(60),
    }
}

/// A fully allocated gossip cluster ready to run protocol rounds.
pub struct GossipSimulation {
    nodes: Vec<MustardNode<InMemoryTransport>>,
    node_ids: Vec<NodeId>,
    node_indices: HashMap<NodeId, u32>,
    known_members: Vec<Vec<u32>>,
}

impl GossipSimulation {
    /// Allocate `cluster_size` nodes and connect them in a seed ring.
    pub async fn new(cluster_size: usize) -> Self {
        assert!(
            cluster_size > 1,
            "gossip simulation needs at least two nodes"
        );
        assert!(
            u32::try_from(cluster_size).is_ok(),
            "gossip simulation supports at most {} nodes",
            u32::MAX
        );
        let network = InMemoryNetwork::new();
        let config = fast_config();
        let mut nodes = Vec::with_capacity(cluster_size);
        let mut addresses = Vec::with_capacity(cluster_size);
        let node_ids: Vec<_> = (0..cluster_size)
            .map(|index| NodeId::new(format!("n{index}")))
            .collect();
        let node_indices = node_ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, node_id)| (node_id, index as u32))
            .collect();

        for (index, node_id) in node_ids.iter().enumerate() {
            let address = node_addr(index);
            addresses.push(address);
            let transport = network.register(address).await;
            nodes.push(MustardNode::new(
                node_id.clone(),
                address,
                config.clone(),
                transport,
            ));
        }

        for (index, node) in nodes.iter_mut().enumerate() {
            let next = (index + 1) % cluster_size;
            node.add_seed(node_ids[next].clone(), addresses[next]);
        }

        let known_members = (0..cluster_size)
            .map(|index| {
                let mut known = vec![index as u32, ((index + 1) % cluster_size) as u32];
                known.sort_unstable();
                known
            })
            .collect();

        Self {
            nodes,
            node_ids,
            node_indices,
            known_members,
        }
    }

    /// Run protocol rounds until every node knows every member.
    pub async fn converge(&mut self, maximum_rounds: usize) -> Result<usize, String> {
        let cluster_size = self.nodes.len();
        for round in 1..=maximum_rounds {
            for (node_index, node) in self.nodes.iter_mut().enumerate() {
                let known = &self.known_members[node_index];
                if known.len() <= 1 {
                    continue;
                }
                let self_position = known.binary_search(&(node_index as u32)).unwrap();
                let target_position = (round
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(node_index.wrapping_mul(12_345)))
                    % (known.len() - 1);
                let target = if target_position >= self_position {
                    known[target_position + 1]
                } else {
                    known[target_position]
                };

                // The index is kept sorted as messages teach this node about
                // peers. This preserves reproducible, uniform peer selection
                // without allocating and sorting the complete membership on
                // every protocol round.
                let message = GossipMessage::new(
                    node.node_id.clone(),
                    node.incarnation,
                    GossipPayload::Ping {
                        updates: node.dissemination.select_updates(),
                    },
                );
                node.transport
                    .send(node_addr(target as usize), &message)
                    .await
                    .map_err(|error| format!("gossip send failed: {error}"))?;
            }

            for _ in 0..2 {
                for (receiver, node) in self.nodes.iter_mut().enumerate() {
                    while let Some((from, message)) = node.transport.try_recv() {
                        let learned: Vec<_> = std::iter::once(&message.sender)
                            .chain(
                                message
                                    .payload
                                    .updates()
                                    .iter()
                                    .map(|update| &update.node_id),
                            )
                            .filter_map(|node_id| self.node_indices.get(node_id).copied())
                            .collect();
                        node.handle_message(from, message).await;
                        for learned_index in learned {
                            if node
                                .membership
                                .get(&self.node_ids[learned_index as usize])
                                .is_some()
                            {
                                let known = &mut self.known_members[receiver];
                                if let Err(position) = known.binary_search(&learned_index) {
                                    known.insert(position, learned_index);
                                }
                            }
                        }
                    }
                }
            }

            if (round % 10 == 0 || cluster_size <= 100)
                && self
                    .nodes
                    .iter()
                    .all(|node| node.membership.len() == cluster_size)
            {
                return Ok(round);
            }
        }

        let minimum_members = self
            .nodes
            .iter()
            .map(|node| node.membership.len())
            .min()
            .unwrap_or(0);
        Err(format!(
            "did not converge after {maximum_rounds} rounds; minimum membership {minimum_members}/{cluster_size}"
        ))
    }
}

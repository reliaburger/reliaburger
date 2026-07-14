//! Deterministic in-memory gossip simulation shared by benchmarks and the
//! 10,000-node scale acceptance test.

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
}

impl GossipSimulation {
    /// Allocate `cluster_size` nodes and connect them in a seed ring.
    pub async fn new(cluster_size: usize) -> Self {
        assert!(
            cluster_size > 1,
            "gossip simulation needs at least two nodes"
        );
        let network = InMemoryNetwork::new();
        let config = fast_config();
        let mut nodes = Vec::with_capacity(cluster_size);
        let mut addresses = Vec::with_capacity(cluster_size);

        for index in 0..cluster_size {
            let address = node_addr(index);
            addresses.push(address);
            let transport = network.register(address).await;
            nodes.push(MustardNode::new(
                NodeId::new(format!("n{index}")),
                address,
                config.clone(),
                transport,
            ));
        }

        for (index, node) in nodes.iter_mut().enumerate() {
            let next = (index + 1) % cluster_size;
            node.add_seed(NodeId::new(format!("n{next}")), addresses[next]);
        }

        Self { nodes }
    }

    /// Run protocol rounds until every node knows every member.
    pub async fn converge(&mut self, maximum_rounds: usize) -> Result<usize, String> {
        let cluster_size = self.nodes.len();
        for round in 1..=maximum_rounds {
            for (node_index, node) in self.nodes.iter_mut().enumerate() {
                let mut candidates: Vec<_> = node
                    .membership
                    .active_members()
                    .into_iter()
                    .filter(|member| member.node_id != node.node_id)
                    .map(|member| (member.node_id.clone(), member.address))
                    .collect();
                candidates.sort_by(|left, right| left.0.cmp(&right.0));
                if candidates.is_empty() {
                    continue;
                }

                // A reproducible pseudo-random walk. Benchmark runs now use
                // identical peer choices instead of thread_rng noise.
                let target = (round
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(node_index.wrapping_mul(12_345)))
                    % candidates.len();
                let (_, address) = &candidates[target];
                let message = GossipMessage::new(
                    node.node_id.clone(),
                    node.incarnation,
                    GossipPayload::Ping {
                        updates: node.dissemination.select_updates(),
                    },
                );
                node.transport
                    .send(*address, &message)
                    .await
                    .map_err(|error| format!("gossip send failed: {error}"))?;
            }

            for _ in 0..2 {
                for node in &mut self.nodes {
                    while let Some((from, message)) = node.transport.try_recv() {
                        node.handle_message(from, message).await;
                    }
                }
            }

            if (round % 10 == 0 || cluster_size <= 100)
                && self
                    .nodes
                    .iter()
                    .all(|node| node.membership.active_members().len() == cluster_size)
            {
                return Ok(round);
            }
        }

        let minimum_members = self
            .nodes
            .iter()
            .map(|node| node.membership.active_members().len())
            .min()
            .unwrap_or(0);
        Err(format!(
            "did not converge after {maximum_rounds} rounds; minimum membership {minimum_members}/{cluster_size}"
        ))
    }
}

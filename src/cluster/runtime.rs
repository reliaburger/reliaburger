//! Start the cluster runtime and assemble a [`ClusterHandle`].
//!
//! Incremental: this currently wires the gossip layer (real UDP transport,
//! join-by-address, membership convergence). The returned `ClusterHandle`
//! has `council: None` and no Raft/reporting yet — those are layered on in
//! follow-up changes. A node started this way participates in gossip and
//! reports its membership view via `relish nodes`.

use std::net::SocketAddr;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bun::agent::ClusterHandle;
use crate::meat::types::NodeId;
use crate::mustard::config::GossipConfig;
use crate::mustard::membership::MembershipSnapshot;
use crate::mustard::protocol::MustardNode;
use crate::mustard::transport::UdpMustardTransport;
use crate::reporting::worker::CollectSnapshotRequest;

/// Owns resources that must outlive `start()` for the cluster to keep
/// working. Drop it to release them (the spawned tasks also stop when the
/// shared `CancellationToken` is cancelled).
pub struct ClusterRuntime {
    /// Held so the agent's snapshot channel never closes. Until the
    /// reporting worker is wired, nothing sends on it; keeping the sender
    /// alive stops `snapshot_rx.recv()` from returning `None` in a tight
    /// loop inside the agent.
    _snapshot_tx: mpsc::Sender<CollectSnapshotRequest>,
}

/// Configuration for starting the cluster runtime, derived from node config.
pub struct ClusterParams {
    /// This node's gossip name.
    pub node_name: String,
    /// Address to bind gossip on and advertise (ip:gossip_port).
    pub gossip_addr: SocketAddr,
    /// Seed addresses to join (other nodes' gossip endpoints). Empty for the
    /// first/bootstrap node.
    pub seeds: Vec<SocketAddr>,
}

/// Start the gossip layer and return a `ClusterHandle` plus a `ClusterRuntime`
/// holding the resources that must stay alive.
pub async fn start(
    params: ClusterParams,
    shutdown: CancellationToken,
) -> std::io::Result<(ClusterHandle, ClusterRuntime)> {
    let transport = UdpMustardTransport::bind(params.gossip_addr)
        .await
        .map_err(|e| std::io::Error::other(format!("gossip bind failed: {e}")))?;

    let mut node = MustardNode::new(
        NodeId::new(&params.node_name),
        params.gossip_addr,
        GossipConfig::default(),
        transport,
    );
    node.set_seeds(params.seeds);

    let (membership_tx, membership_rx) = watch::channel::<Vec<MembershipSnapshot>>(Vec::new());
    node.set_membership_watch(membership_tx);

    let gossip_shutdown = shutdown.clone();
    tokio::spawn(async move {
        node.run(gossip_shutdown).await;
    });

    let (snapshot_tx, snapshot_rx) = mpsc::channel(16);

    let handle = ClusterHandle {
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
    };

    Ok((
        handle,
        ClusterRuntime {
            _snapshot_tx: snapshot_tx,
        },
    ))
}

//! Start the cluster runtime and assemble a [`ClusterHandle`].
//!
//! Wires the gossip layer (real UDP, join-by-address), the Raft council (real
//! TCP RPC, bootstrap, and a selection loop that grows the council from gossip
//! membership), and the reporting tree (flat star: every node reports its
//! state to the current leader, whose aggregator collects the cluster view).
//! A node started this way gossips, runs Raft, and — once it's the leader —
//! admits other gossiped nodes to the council up to the size cap.
//!
//! The reporting topology is a deliberate MVP: a flat star to the leader. The
//! canonical consistent-hash tree (workers → council member → leader, with
//! multi-level aggregation) is a follow-up; it can replace the star without
//! changing this interface.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bun::agent::ClusterHandle;
use crate::cluster::identity;
use crate::config::node::ReportingTreeSection;
use crate::council::log_store::MemLogStore;
use crate::council::network::{TcpRaftNetworkFactory, serve_raft_rpc};
use crate::council::node::CouncilNode;
use crate::council::state_machine::CouncilStateMachine;
use crate::council::types::{CouncilConfig, CouncilNodeInfo};
use crate::meat::types::NodeId;
use crate::mustard::config::GossipConfig;
use crate::mustard::membership::MembershipSnapshot;
use crate::mustard::protocol::MustardNode;
use crate::mustard::state::NodeState;
use crate::mustard::transport::UdpMustardTransport;
use crate::reporting::aggregator::{AggregatedState, ReportAggregator};
use crate::reporting::transport::TcpReportingTransport;
use crate::reporting::worker::ReportWorker;

/// Upper bound on council (Raft voter) size. Beyond this, alive nodes stay
/// workers. Matches `CouncilSelectionConfig`'s default `max_council_size`.
const MAX_COUNCIL_SIZE: usize = 7;

/// How often the leader reconciles the council against gossip membership.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// Owns resources that must outlive `start()` for the cluster to keep
/// working, and exposes the local reporting aggregator's view. The spawned
/// tasks stop when the shared `CancellationToken` is cancelled.
pub struct ClusterRuntime {
    /// This node's reporting aggregator view. Only meaningful on the leader
    /// (the flat-star topology has every node report to the leader), where it
    /// holds the latest state report from every node.
    pub aggregated_rx: watch::Receiver<AggregatedState>,
}

/// Configuration for starting the cluster runtime, derived from node config.
pub struct ClusterParams {
    /// This node's gossip name.
    pub node_name: String,
    /// Address to bind gossip on and advertise (ip:gossip_port).
    pub gossip_addr: SocketAddr,
    /// Port for the Raft RPC server (same IP as gossip).
    pub raft_port: u16,
    /// Port for the reporting-tree transport (same IP as gossip).
    pub reporting_port: u16,
    /// Reporting-tree timing config.
    pub reporting_config: ReportingTreeSection,
    /// Seed addresses to join (other nodes' gossip endpoints). Empty for the
    /// first/bootstrap node.
    pub seeds: Vec<SocketAddr>,
    /// Master secret for unwrapping CA keys during council operations.
    pub wrapping_ikm: Option<[u8; 32]>,
}

/// Start gossip + the Raft council and return a `ClusterHandle` plus a
/// `ClusterRuntime` holding resources that must stay alive.
pub async fn start(
    params: ClusterParams,
    shutdown: CancellationToken,
) -> std::io::Result<(ClusterHandle, ClusterRuntime)> {
    // --- Gossip ---
    let transport = UdpMustardTransport::bind(params.gossip_addr)
        .await
        .map_err(|e| std::io::Error::other(format!("gossip bind failed: {e}")))?;

    let mut node = MustardNode::new(
        NodeId::new(&params.node_name),
        params.gossip_addr,
        GossipConfig::default(),
        transport,
    );
    node.set_seeds(params.seeds.clone());

    let (membership_tx, membership_rx) = watch::channel::<Vec<MembershipSnapshot>>(Vec::new());
    node.set_membership_watch(membership_tx);

    let gossip_shutdown = shutdown.clone();
    tokio::spawn(async move {
        node.run(gossip_shutdown).await;
    });

    // --- Raft council ---
    // Every clustered node runs a CouncilNode and a Raft RPC server so the
    // leader can admit it without a separate handshake. The first node (no
    // seeds) bootstraps itself; others wait to be added by the leader's
    // reconciler. Workers beyond the size cap simply never become voters.
    let raft_id = identity::raft_id_from_name(&params.node_name);
    let raft_addr = SocketAddr::new(params.gossip_addr.ip(), params.raft_port);
    let self_info = CouncilNodeInfo {
        addr: raft_addr,
        name: params.node_name.clone(),
    };
    // Raft port relative to gossip port; used to derive each peer's Raft
    // address from its advertised gossip address (see identity::council_info).
    let port_offset = params.raft_port as i32 - params.gossip_addr.port() as i32;

    let factory = TcpRaftNetworkFactory::new(raft_id);
    let council = CouncilNode::new(
        raft_id,
        CouncilConfig::default(),
        factory,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        params.wrapping_ikm,
    )
    .await
    .map_err(|e| std::io::Error::other(format!("council init failed: {e}")))?;
    let council = Arc::new(council);

    let raft_listener = tokio::net::TcpListener::bind(raft_addr).await?;
    let raft = council.raft().clone();
    let rpc_shutdown = shutdown.clone();
    tokio::spawn(async move {
        serve_raft_rpc(raft_listener, raft, rpc_shutdown).await;
    });

    // Bootstrap: the first node initialises a single-member cluster and wins
    // the election immediately. Best-effort — if already initialised, ignore.
    if params.seeds.is_empty() {
        let mut members = BTreeMap::new();
        members.insert(raft_id, self_info.clone());
        let _ = council.initialize(members).await;
    }

    let raft_metrics_rx = council.metrics();

    spawn_council_reconciler(
        Arc::clone(&council),
        membership_rx.clone(),
        port_offset,
        raft_id,
        self_info,
        shutdown.clone(),
    );

    // --- Reporting tree (flat star: every node reports to the leader) ---
    let reporting_offset = params.reporting_port as i32 - params.raft_port as i32;
    let reporting_addr = SocketAddr::new(params.gossip_addr.ip(), params.reporting_port);

    // Aggregator: every node listens, but only the leader actually receives
    // reports (workers target the leader), so a leadership change needs no
    // start/stop dance — the new leader's aggregator is already running.
    let agg_transport = TcpReportingTransport::bind(reporting_addr, shutdown.clone())
        .await
        .map_err(|e| std::io::Error::other(format!("reporting bind failed: {e}")))?;
    let (mut aggregator, aggregated_rx) = ReportAggregator::new(
        agg_transport,
        params.reporting_config.clone(),
        shutdown.clone(),
        None,
    );
    tokio::spawn(async move { aggregator.run().await });

    // A maintainer keeps `council_rx` pointing at the current leader's
    // reporting address; the worker reports there.
    let (council_tx, council_rx) = watch::channel::<Vec<(NodeId, SocketAddr)>>(Vec::new());
    spawn_leader_target_maintainer(
        raft_metrics_rx.clone(),
        council_tx,
        reporting_offset,
        shutdown.clone(),
    );

    // Worker: snapshots this node's state (via the agent) and sends it to the
    // leader. Binds an ephemeral port — it only sends; replies are ignored.
    let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let worker_transport = TcpReportingTransport::bind(
        SocketAddr::new(params.gossip_addr.ip(), 0),
        shutdown.clone(),
    )
    .await
    .map_err(|e| std::io::Error::other(format!("reporting worker bind failed: {e}")))?;
    let mut worker = ReportWorker::new(
        NodeId::new(&params.node_name),
        worker_transport,
        params.reporting_config.clone(),
        snapshot_tx,
        council_rx,
        shutdown.clone(),
    );
    tokio::spawn(async move { worker.run().await });

    let handle = ClusterHandle {
        membership_rx,
        raft_metrics_rx: Some(raft_metrics_rx),
        council: Some(council),
        snapshot_rx,
        wrapping_ikm: params.wrapping_ikm,
    };

    Ok((handle, ClusterRuntime { aggregated_rx }))
}

/// Keep `council_tx` pointing at the current Raft leader's reporting address
/// (a single-element council list), so the flat-star worker always reports to
/// the leader. Recomputes on every metrics change.
fn spawn_leader_target_maintainer(
    mut metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    council_tx: watch::Sender<Vec<(NodeId, SocketAddr)>>,
    reporting_offset: i32,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let target = {
                let m = metrics_rx.borrow();
                m.current_leader.and_then(|leader_id| {
                    m.membership_config
                        .membership()
                        .get_node(&leader_id)
                        .map(|info| {
                            let raft = info.addr;
                            let reporting = SocketAddr::new(
                                raft.ip(),
                                (raft.port() as i32 + reporting_offset) as u16,
                            );
                            vec![(NodeId::new(&info.name), reporting)]
                        })
                })
            };
            if let Some(target) = target {
                let _ = council_tx.send(target);
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = metrics_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Spawn the council reconciler: on the leader, periodically bring the Raft
/// voter set in line with gossip membership (self + alive nodes, capped).
/// No-ops on followers and when nothing has changed, so it's safe to run on
/// every node.
fn spawn_council_reconciler(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    port_offset: i32,
    self_id: u64,
    self_info: CouncilNodeInfo,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    if !council.is_leader().await {
                        continue;
                    }

                    // Desired voters: self plus every alive gossiped node,
                    // each at its OWN derived Raft address, ordered by raft
                    // id and capped.
                    let snapshot = membership_rx.borrow().clone();
                    let mut desired: Vec<(u64, CouncilNodeInfo)> = vec![(self_id, self_info.clone())];
                    for member in &snapshot {
                        if member.state != NodeState::Alive {
                            continue;
                        }
                        let (id, info) = identity::council_info(member, port_offset);
                        if id != self_id && !desired.iter().any(|(d, _)| *d == id) {
                            desired.push((id, info));
                        }
                    }
                    desired.sort_by_key(|(id, _)| *id);
                    desired.truncate(MAX_COUNCIL_SIZE);
                    let desired_ids: BTreeSet<u64> =
                        desired.iter().map(|(id, _)| *id).collect();

                    let current: BTreeSet<u64> = council
                        .metrics()
                        .borrow()
                        .membership_config
                        .membership()
                        .voter_ids()
                        .collect();

                    if desired_ids == current {
                        continue;
                    }

                    // Add any new members as learners (blocking until they
                    // catch up), then promote the whole set to voters.
                    for (id, info) in &desired {
                        if !current.contains(id) {
                            let _ = council.add_learner(*id, info.clone()).await;
                        }
                    }
                    let _ = council.change_membership(desired_ids).await;
                }
            }
        }
    });
}

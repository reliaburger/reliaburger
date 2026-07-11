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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bun::agent::ClusterHandle;
use crate::cluster::identity;
use crate::config::node::ReportingTreeSection;
use crate::council::network::{TcpRaftNetworkFactory, serve_raft_rpc};
use crate::council::node::CouncilNode;
use crate::council::selection::{CouncilSelectionConfig, select_council_candidates};
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

/// Upper bound on a single reconciler membership operation. A peer whose Raft
/// port is unreachable must not block the reconciler forever — on timeout we
/// log and retry on the next tick.
const RECONCILE_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns resources that must outlive `start()` for the cluster to keep
/// working, and exposes the local reporting aggregator's view. The spawned
/// tasks stop when the shared `CancellationToken` is cancelled.
pub struct ClusterRuntime {
    /// This node's reporting aggregator view. Only meaningful on the leader
    /// (the flat-star topology has every node report to the leader), where it
    /// holds the latest state report from every node.
    pub aggregated_rx: watch::Receiver<AggregatedState>,
    /// Rollup store fed by the aggregator from incoming `MetricsRollup`
    /// messages. Only populated on the leader (same flat-star rule);
    /// the API serves `/v1/metrics/cluster` from it.
    pub rollup_store: Arc<tokio::sync::RwLock<crate::mayo::rollup_store::RollupStore>>,
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
    /// Initial `SecurityState` to seed into Raft on a fresh bootstrap. Only the
    /// bootstrap node sets this; it replicates to everyone else through Raft.
    pub bootstrap_security_state: Option<Box<crate::sesame::types::SecurityState>>,
    /// Node data directory; the durable Raft log/snapshot live under `{data_dir}/raft/`.
    pub data_dir: std::path::PathBuf,
    /// Local Mayo metrics store. When set, a `RollupWorker` pushes
    /// per-node rollups to the leader on `rollup_interval`.
    pub mayo: Option<Arc<tokio::sync::RwLock<crate::mayo::store::MayoStore>>>,
    /// How often the rollup worker pushes (from `[metrics] rollup_interval_secs`).
    pub rollup_interval: Duration,
    /// This node's mTLS identity. When set, the Raft RPC listener requires
    /// client certificates and peers are dialled over mTLS. `None` keeps the
    /// internal transports plaintext (the caller enforces `require_mtls`).
    pub identity: Option<Arc<crate::sesame::identity_store::NodeIdentity>>,
}

/// Start gossip + the Raft council and return a `ClusterHandle` plus a
/// `ClusterRuntime` holding resources that must stay alive.
pub async fn start(
    params: ClusterParams,
    shutdown: CancellationToken,
) -> std::io::Result<(ClusterHandle, ClusterRuntime)> {
    // --- Gossip ---
    // Authenticate gossip datagrams with an HMAC keyed by the shared master
    // secret (every node derives the same key). Nodes without a master key
    // (single-node / pre-security) get None and gossip in the clear as before.
    let gossip_key = params
        .wrapping_ikm
        .as_ref()
        .map(crate::sesame::mtls::gossip_hmac::derive_gossip_key);
    let transport = UdpMustardTransport::bind(params.gossip_addr)
        .await
        .map_err(|e| std::io::Error::other(format!("gossip bind failed: {e}")))?
        .with_key(gossip_key);
    // Grab the gossip blocklist before the transport moves into the
    // node — chaos partitions populate it to silently drop datagrams.
    let gossip_blocklist = transport.blocklist();

    let mut node = MustardNode::new(
        NodeId::new(&params.node_name),
        params.gossip_addr,
        GossipConfig::default(),
        transport,
    );
    node.set_seeds(params.seeds.clone());

    let (membership_tx, membership_rx) = watch::channel::<Vec<MembershipSnapshot>>(Vec::new());
    node.set_membership_watch(membership_tx);
    // The gossip node is spawned *after* the council is built, so a restarted
    // node can seed gossip from its restored Raft membership (see below).

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

    // Durable Raft storage: the log/vote (log.redb) and the state-machine
    // snapshot (snapshot.redb) live under {data_dir}/raft/ so the node
    // remembers its vote/log across restarts (Raft safety) instead of forming a
    // fresh cluster.
    let raft_dir = params.data_dir.join("raft");
    std::fs::create_dir_all(&raft_dir)?;
    let log_store =
        crate::council::durable_log::DurableLogStore::open(raft_dir.join("log.redb"))
            .map_err(|e| std::io::Error::other(format!("raft log store open failed: {e}")))?;
    // Whether this node has never persisted any Raft state — drives the
    // bootstrap guard below.
    let store_fresh = log_store.is_fresh().await;
    let snapshot_db = Arc::new(
        redb::Database::create(raft_dir.join("snapshot.redb"))
            .map_err(|e| std::io::Error::other(format!("raft snapshot store open failed: {e}")))?,
    );
    let state_machine = CouncilStateMachine::with_store(snapshot_db)
        .map_err(|e| std::io::Error::other(format!("raft snapshot store load failed: {e}")))?;

    // Build the shared CRL handle (seeded from the bootstrap security state
    // if present) and, when this node has an identity, the mTLS acceptor and
    // connector for the internal transports. bun's security refresh ticker
    // updates `crl_handle` as `RevokeCertificate` entries replicate.
    let crl_handle = crate::sesame::mtls::CrlHandle::new(
        params
            .bootstrap_security_state
            .as_ref()
            .map(|s| s.crl.clone())
            .unwrap_or_default(),
    );
    let (raft_acceptor, raft_connector) = match &params.identity {
        Some(identity) => {
            let server =
                crate::sesame::mtls::build_mtls_server_config(identity, crl_handle.clone())
                    .map_err(|e| std::io::Error::other(format!("mTLS server config: {e}")))?;
            let client =
                crate::sesame::mtls::build_mtls_client_config(identity, crl_handle.clone())
                    .map_err(|e| std::io::Error::other(format!("mTLS client config: {e}")))?;
            (
                Some(tokio_rustls::TlsAcceptor::from(server)),
                Some(tokio_rustls::TlsConnector::from(client)),
            )
        }
        None => (None, None),
    };

    let factory = match raft_connector.clone() {
        Some(connector) => TcpRaftNetworkFactory::new_tls(raft_id, connector),
        None => TcpRaftNetworkFactory::new(raft_id),
    };
    // Same for the Raft RPC blocklist — a partition must cut both the
    // gossip and Raft transports or SWIM half-detects the peer.
    let raft_blocklist = factory.blocklist();
    let council = CouncilNode::new(
        raft_id,
        CouncilConfig::default(),
        factory,
        log_store,
        state_machine,
        params.wrapping_ikm,
    )
    .await
    .map_err(|e| std::io::Error::other(format!("council init failed: {e}")))?;
    let council = Arc::new(council);

    // On restart (no configured seeds, but a populated durable store), seed
    // gossip from the RESTORED Raft membership. A restarted seeds-empty node
    // otherwise comes up knowing no peers, so its perimeter firewall keeps the
    // cluster ports closed and it never reconverges. Re-probing the restored
    // peers (gossip is UDP, unfirewalled) reopens the firewall and lets Raft
    // reconnect. Enabled by C3's durable membership.
    if params.seeds.is_empty() && !store_fresh {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut seeds: Vec<SocketAddr> = Vec::new();
        loop {
            seeds.clear();
            for (id, info) in council
                .metrics()
                .borrow()
                .membership_config
                .membership()
                .nodes()
            {
                if *id != raft_id {
                    let gossip_port = (info.addr.port() as i32 - port_offset) as u16;
                    seeds.push(SocketAddr::new(info.addr.ip(), gossip_port));
                }
            }
            // Wait briefly for openraft to finish loading the restored membership.
            if !seeds.is_empty() || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !seeds.is_empty() {
            node.set_seeds(seeds);
        }
    }

    // Now start gossip.
    let gossip_shutdown = shutdown.clone();
    tokio::spawn(async move {
        node.run(gossip_shutdown).await;
    });

    let raft_listener = tokio::net::TcpListener::bind(raft_addr).await?;
    let raft = council.raft().clone();
    let rpc_shutdown = shutdown.clone();
    let rpc_acceptor = raft_acceptor.clone();
    tokio::spawn(async move {
        serve_raft_rpc(raft_listener, raft, rpc_shutdown, rpc_acceptor).await;
    });

    // Bootstrap ONLY a genuinely new cluster: no seeds AND a fresh durable
    // store. A restarted seed node has a populated store, so it resumes its
    // existing cluster from durable state instead of re-initialising into a
    // fresh single-node cluster (which would elect itself a second leader —
    // the split-brain bug this stage fixes).
    if params.seeds.is_empty() && store_fresh {
        let mut members = BTreeMap::new();
        members.insert(raft_id, self_info.clone());
        let _ = council.initialize(members).await;

        // Seed the initial SecurityState (CAs, age keys, OIDC config) into Raft
        // once, on this bootstrap node. It then replicates to every node that
        // joins. Durable Raft (C3) makes this idempotent: a restart has a
        // populated store, skips this whole block, and never re-seeds.
        if let Some(state) = &params.bootstrap_security_state
            && let Err(e) = seed_bootstrap_state(&council, state).await
        {
            // The cluster still forms without CA material; log loudly rather
            // than abort so a seed failure is diagnosable, not silent.
            eprintln!("cluster: failed to seed security bootstrap state: {e}");
        }
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
    let agg_transport = TcpReportingTransport::bind_tls(
        reporting_addr,
        shutdown.clone(),
        raft_acceptor.clone(),
        raft_connector.clone(),
    )
    .await
    .map_err(|e| std::io::Error::other(format!("reporting bind failed: {e}")))?;
    // The rollup store lives on every node (cheap when empty) so a
    // leadership change needs no start/stop dance — only the leader's
    // aggregator actually receives rollups to ingest into it.
    let rollup_store = Arc::new(tokio::sync::RwLock::new(
        crate::mayo::rollup_store::RollupStore::new(params.data_dir.join("rollups")),
    ));
    let (mut aggregator, aggregated_rx) = ReportAggregator::new(
        agg_transport,
        params.reporting_config.clone(),
        shutdown.clone(),
        Some(Arc::clone(&rollup_store)),
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
    let worker_transport = TcpReportingTransport::bind_tls(
        SocketAddr::new(params.gossip_addr.ip(), 0),
        shutdown.clone(),
        raft_acceptor.clone(),
        raft_connector.clone(),
    )
    .await
    .map_err(|e| std::io::Error::other(format!("reporting worker bind failed: {e}")))?;
    let rollup_council_rx = council_rx.clone();
    let mut worker = ReportWorker::new(
        NodeId::new(&params.node_name),
        worker_transport,
        params.reporting_config.clone(),
        snapshot_tx,
        council_rx,
        shutdown.clone(),
    );
    tokio::spawn(async move { worker.run().await });

    // Rollup worker: pushes this node's metric rollups to the leader,
    // where the aggregator ingests them into the rollup store.
    if let Some(mayo) = params.mayo.clone() {
        let rollup_transport = TcpReportingTransport::bind_tls(
            SocketAddr::new(params.gossip_addr.ip(), 0),
            shutdown.clone(),
            raft_acceptor.clone(),
            raft_connector.clone(),
        )
        .await
        .map_err(|e| std::io::Error::other(format!("rollup worker bind failed: {e}")))?;
        let mut rollup_worker = crate::mayo::rollup_worker::RollupWorker::new(
            NodeId::new(&params.node_name),
            rollup_transport,
            mayo,
            rollup_council_rx,
            params.rollup_interval,
            shutdown.clone(),
        );
        tokio::spawn(async move { rollup_worker.run().await });
    }

    let handle = ClusterHandle {
        membership_rx,
        raft_metrics_rx: Some(raft_metrics_rx),
        council: Some(council),
        snapshot_rx,
        wrapping_ikm: params.wrapping_ikm,
        partition_blocklists: crate::bun::agent::PartitionBlocklists {
            gossip: Some(gossip_blocklist),
            raft: Some(raft_blocklist),
            raft_port_offset: port_offset,
        },
        crl_handle,
    };

    Ok((
        handle,
        ClusterRuntime {
            aggregated_rx,
            rollup_store,
        },
    ))
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

/// Seed the initial `SecurityState` into Raft on a freshly bootstrapped
/// cluster.
///
/// Called once, on the bootstrap node, immediately after `initialize`. Retries
/// briefly while leadership settles: `client_write` can race the election that
/// `initialize` kicks off and briefly return `ForwardToLeader` before this node
/// finishes becoming leader.
async fn seed_bootstrap_state(
    council: &CouncilNode,
    state: &crate::sesame::types::SecurityState,
) -> Result<(), crate::council::CouncilError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let request =
            crate::council::types::RaftRequest::SecurityStateInit(Box::new(state.clone()));
        match council.write(request).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Spawn the council reconciler: on the leader, periodically bring the Raft
/// voter set in line with gossip membership (self + alive nodes, capped).
/// No-ops on followers and when nothing has changed, so it's safe to run on
/// every node.
/// Compute the desired council voter set from the current voters and gossip
/// membership.
///
/// Current voters are ALWAYS retained (the leader is never demoted — the L5
/// bug), and `select_council_candidates` grows the set toward the cap by zone
/// diversity and node age. Returns the desired voter ids and the members that
/// still need to be added as learners.
#[allow(clippy::too_many_arguments)]
fn compute_desired_council(
    current: &BTreeSet<u64>,
    snapshot: &[MembershipSnapshot],
    self_id: u64,
    self_info: &CouncilNodeInfo,
    port_offset: i32,
    config: &CouncilSelectionConfig,
    now: Instant,
) -> (BTreeSet<u64>, Vec<(u64, CouncilNodeInfo)>) {
    // Map each raft id to its gossip node id + derived raft info.
    let mut by_raft: HashMap<u64, (NodeId, CouncilNodeInfo)> = HashMap::new();
    by_raft.insert(
        self_id,
        (NodeId::new(self_info.name.clone()), self_info.clone()),
    );
    for member in snapshot {
        if member.state != NodeState::Alive {
            continue;
        }
        let (rid, info) = identity::council_info(member, port_offset);
        by_raft
            .entry(rid)
            .or_insert_with(|| (member.node_id.clone(), info));
    }

    let current_nodeids: Vec<NodeId> = current
        .iter()
        .filter_map(|rid| by_raft.get(rid).map(|(nid, _)| nid.clone()))
        .collect();
    let selected =
        select_council_candidates(snapshot, &current_nodeids, MAX_COUNCIL_SIZE, config, now);

    let mut desired_ids = current.clone();
    let mut to_add: Vec<(u64, CouncilNodeInfo)> = Vec::new();
    for nid in &selected {
        let rid = identity::raft_id_from_name(&nid.0);
        if let Some((_, info)) = by_raft.get(&rid)
            && desired_ids.insert(rid)
        {
            to_add.push((rid, info.clone()));
        }
    }
    (desired_ids, to_add)
}

fn spawn_council_reconciler(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    port_offset: i32,
    self_id: u64,
    self_info: CouncilNodeInfo,
    shutdown: CancellationToken,
) {
    // Council selection config for the reconciler. min_node_age is 0 so a fresh
    // cluster can form a council immediately; age still affects *ordering*
    // (older nodes preferred), just not eligibility.
    let selection_config = CouncilSelectionConfig {
        min_node_age: Duration::from_secs(0),
        ..Default::default()
    };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    if !council.is_leader().await {
                        continue;
                    }

                    let snapshot = membership_rx.borrow().clone();

                    let current: BTreeSet<u64> = council
                        .metrics()
                        .borrow()
                        .membership_config
                        .membership()
                        .voter_ids()
                        .collect();

                    let (desired_ids, desired) = compute_desired_council(
                        &current,
                        &snapshot,
                        self_id,
                        &self_info,
                        port_offset,
                        &selection_config,
                        Instant::now(),
                    );

                    if desired_ids == current {
                        continue;
                    }

                    // Add any new members as learners, then promote the whole
                    // set to voters. Each op is bounded by a timeout and its
                    // error is logged (not discarded) so an unreachable peer
                    // can't wedge the reconciler — it simply retries next tick.
                    for (id, info) in &desired {
                        if !current.contains(id) {
                            match tokio::time::timeout(
                                RECONCILE_OP_TIMEOUT,
                                council.add_learner(*id, info.clone()),
                            )
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    eprintln!("council reconciler: add_learner({id}) failed: {e}");
                                }
                                Err(_) => {
                                    eprintln!(
                                        "council reconciler: add_learner({id}) timed out; will retry"
                                    );
                                }
                            }
                        }
                    }
                    match tokio::time::timeout(
                        RECONCILE_OP_TIMEOUT,
                        council.change_membership(desired_ids),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            eprintln!("council reconciler: change_membership failed: {e}");
                        }
                        Err(_) => {
                            eprintln!("council reconciler: change_membership timed out; will retry");
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::cluster::identity::raft_id_from_name;
    use crate::mustard::state::NodeState;

    fn snap(name: &str, port: u16, now: Instant) -> MembershipSnapshot {
        MembershipSnapshot {
            node_id: NodeId::new(name),
            address: format!("127.0.0.1:{port}").parse().unwrap(),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
            first_seen: now,
            resources: None,
        }
    }

    fn info(name: &str, port: u16) -> CouncilNodeInfo {
        CouncilNodeInfo {
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
            name: name.to_string(),
        }
    }

    #[test]
    fn compute_desired_retains_leader_and_grows() {
        let now = Instant::now();
        let self_info = info("leader", 9444);
        let self_id = raft_id_from_name("leader");
        // The leader is the only current voter.
        let current = BTreeSet::from([self_id]);

        // Gossip: leader + two alive peers.
        let snapshot = vec![
            snap("leader", 9443, now),
            snap("peer-a", 9445, now),
            snap("peer-b", 9447, now),
        ];
        let config = CouncilSelectionConfig {
            min_node_age: Duration::from_secs(0),
            ..Default::default()
        };

        let (desired, to_add) =
            compute_desired_council(&current, &snapshot, self_id, &self_info, 1, &config, now);

        // Leader-safety invariant: every current voter is still desired.
        assert!(
            current.is_subset(&desired),
            "the leader must never be demoted"
        );
        // And the council grew to the minimum size using the peers.
        assert_eq!(desired.len(), 3);
        assert!(to_add.iter().all(|(rid, _)| *rid != self_id));
    }

    /// Build a single-node in-memory council, initialised so it becomes leader.
    async fn single_bootstrap_council() -> CouncilNode {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};

        let router = InMemoryRaftRouter::new();
        let id = 1u64;
        let network = InMemoryRaftNetworkFactory::new(id, router.clone());
        let node = CouncilNode::new(
            id,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(id, node.raft().clone()).await;
        let mut members = BTreeMap::new();
        members.insert(id, info("node-1", 9444));
        node.initialize(members).await.unwrap();
        node
    }

    #[tokio::test]
    async fn seed_bootstrap_state_writes_security_state_to_raft() {
        let council = single_bootstrap_council().await;
        let dir = std::env::temp_dir().join("rb-runtime-seed-write");
        std::fs::create_dir_all(&dir).unwrap();
        let init = crate::sesame::init::initialize_cluster("seedwrite", "node-1", &dir).unwrap();

        seed_bootstrap_state(&council, &init.security_state)
            .await
            .unwrap();

        let state = council.security_state().await;
        assert_eq!(state.certificate_authorities.len(), 4);
        assert!(state.cluster_age_keypair().is_some());
        assert!(state.oidc_signing_config.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn seed_bootstrap_state_is_idempotent_on_reapply() {
        let council = single_bootstrap_council().await;
        let dir = std::env::temp_dir().join("rb-runtime-seed-idem");
        std::fs::create_dir_all(&dir).unwrap();
        let init = crate::sesame::init::initialize_cluster("seedidem", "node-1", &dir).unwrap();

        // The apply arm overwrites, so re-seeding leaves one coherent state.
        seed_bootstrap_state(&council, &init.security_state)
            .await
            .unwrap();
        seed_bootstrap_state(&council, &init.security_state)
            .await
            .unwrap();

        let state = council.security_state().await;
        assert_eq!(state.certificate_authorities.len(), 4);
        assert_eq!(
            state.age_keypairs.len(),
            init.security_state.age_keypairs.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compute_desired_noop_when_already_at_target() {
        let now = Instant::now();
        let self_info = info("leader", 9444);
        let self_id = raft_id_from_name("leader");
        let a = raft_id_from_name("peer-a");
        let b = raft_id_from_name("peer-b");
        let current = BTreeSet::from([self_id, a, b]);

        let snapshot = vec![
            snap("leader", 9443, now),
            snap("peer-a", 9445, now),
            snap("peer-b", 9447, now),
        ];
        let config = CouncilSelectionConfig {
            min_node_age: Duration::from_secs(0),
            ..Default::default()
        };

        let (desired, to_add) =
            compute_desired_council(&current, &snapshot, self_id, &self_info, 1, &config, now);

        // Already at the minimum council size with these three — nothing to add.
        assert_eq!(desired, current);
        assert!(to_add.is_empty());
    }
}

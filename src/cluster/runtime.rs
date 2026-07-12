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
use crate::council::selection::{
    CouncilAction, CouncilObservation, CouncilSelectionConfig, HealthTracker, ObservedMember,
    plan_council_action, select_council_candidates,
};
use crate::council::state_machine::CouncilStateMachine;
use crate::council::types::{CouncilConfig, CouncilNodeInfo};
use crate::meat::types::NodeId;
use crate::mustard::config::GossipConfig;
use crate::mustard::directory::NodeDirectory;
use crate::mustard::membership::MembershipSnapshot;
use crate::mustard::message::LeaderHint;
use crate::mustard::protocol::MustardNode;
use crate::mustard::state::NodeState;
use crate::mustard::transport::UdpMustardTransport;
use crate::reporting::aggregator::{AggregatedState, ReportAggregator};
use crate::reporting::transport::TcpReportingTransport;
use crate::reporting::worker::ReportWorker;

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
    /// The gossip-learned control-plane directory: per-node advertised
    /// endpoints and the best leader hint. This is what lets non-voters
    /// find the leader (Phase 12b.2, H1).
    pub directory_rx: watch::Receiver<NodeDirectory>,
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
    /// Port the HTTP API listens on (same IP as gossip). Advertised via the
    /// gossip directory so peers stop deriving it by offset arithmetic.
    pub api_port: u16,
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
    /// Encrypted external council backup (`[cluster.backup]`, 12b.2 D21/CP12).
    /// The leader-only export loop runs when `url` is set and a master key is
    /// available to seal with.
    pub backup: crate::council::backup::BackupConfig,
    /// This node's placement labels (`[node] labels` in node.toml).
    /// Advertised (bounded) via the gossip directory so remote members can
    /// filter on them and zone-aware council selection has real input (CP7).
    pub labels: std::collections::BTreeMap<String, String>,
}

/// Open and validate the durable Raft stores under `raft_dir`.
///
/// This is the startup gate for consensus persistence (CP3). Everything
/// suspicious is fatal here, before Raft starts: an unreadable log store is
/// an error rather than "fresh" (re-bootstrapping would split-brain), a
/// present snapshot must pass its checksum and version check, and the
/// snapshot must cover everything the log has purged. Returns the log store,
/// whether the durable store is genuinely fresh (drives the bootstrap
/// guard), and the loaded state machine.
pub async fn open_raft_storage(
    raft_dir: &std::path::Path,
) -> std::io::Result<(
    crate::council::durable_log::DurableLogStore,
    bool,
    CouncilStateMachine,
)> {
    std::fs::create_dir_all(raft_dir)?;
    let log_path = raft_dir.join("log.redb");
    let log_store = crate::council::durable_log::DurableLogStore::open(&log_path)
        .map_err(|e| std::io::Error::other(format!("raft log store open failed: {e}")))?;
    let store_fresh = log_store.is_fresh().map_err(|e| {
        std::io::Error::other(format!(
            "raft log store at {} is unreadable, refusing to treat it as fresh: {e}",
            log_path.display()
        ))
    })?;
    let snapshot_path = raft_dir.join("snapshot.redb");
    let snapshot_db = Arc::new(redb::Database::create(&snapshot_path).map_err(|e| {
        std::io::Error::other(format!(
            "raft snapshot store open failed at {}: {e}",
            snapshot_path.display()
        ))
    })?);
    let state_machine = CouncilStateMachine::with_store(snapshot_db).map_err(|e| {
        std::io::Error::other(format!(
            "raft snapshot store load failed at {}: {e}",
            snapshot_path.display()
        ))
    })?;
    crate::council::validate_purge_boundary(&log_store, &state_machine)
        .await
        .map_err(|e| {
            std::io::Error::other(format!(
                "raft storage validation failed at {}: {e}",
                raft_dir.display()
            ))
        })?;
    Ok((log_store, store_fresh, state_machine))
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

    // Control-plane directory (12b.2): every datagram this node sends
    // advertises its API and reporting endpoints, plus the best leader hint
    // it knows. Received extensions accumulate into `directory_rx`, which is
    // how nodes OUTSIDE the Raft voter set learn who leads and where — local
    // Raft metrics only carry that for voters. The hint channel is fed by
    // the publisher task below once the council exists.
    node.set_advertised_endpoints(
        params.api_port,
        params.reporting_port,
        params.labels.clone(),
    );
    let (leader_hint_tx, leader_hint_rx) = watch::channel::<Option<LeaderHint>>(None);
    node.set_leader_hint_watch(leader_hint_rx);
    let (directory_tx, directory_rx) = watch::channel(NodeDirectory::default());
    node.set_directory_watch(directory_tx);
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
    // fresh cluster. `store_fresh` drives the bootstrap guard below.
    let raft_dir = params.data_dir.join("raft");
    let (log_store, store_fresh, state_machine) = open_raft_storage(&raft_dir).await?;

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
    spawn_supervised("gossip", shutdown.clone(), async move {
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

    // Encrypted external council backup (12b.2 D21/CP12): a leader-only export
    // loop, only when a destination is configured and a master key is present
    // to seal with. Without the master key there is nothing to derive the seal
    // key from, so we skip it loudly rather than shipping plaintext state.
    if params.backup.enabled() {
        match params.wrapping_ikm {
            Some(master_key) => {
                tokio::spawn(crate::council::backup::run_backup_loop(
                    Arc::clone(&council),
                    master_key,
                    params.backup.clone(),
                    shutdown.clone(),
                ));
            }
            None => eprintln!(
                "cluster: council backup configured but no master key available; backups disabled"
            ),
        }
    }

    // --- Reporting tree (flat star: every node reports to the leader) ---
    let reporting_offset = params.reporting_port as i32 - params.raft_port as i32;
    let api_offset = params.api_port as i32 - params.raft_port as i32;
    let reporting_addr = SocketAddr::new(params.gossip_addr.ip(), params.reporting_port);
    let api_addr = SocketAddr::new(params.gossip_addr.ip(), params.api_port);

    // Leader hint publisher: while THIS node is the Raft leader, gossip
    // carries a hint naming it (with its advertised endpoints and term).
    // Every other node relays the highest-term hint, so workers outside the
    // council learn the leader without Raft metrics.
    spawn_leader_hint_publisher(
        raft_metrics_rx.clone(),
        leader_hint_tx,
        raft_id,
        NodeId::new(&params.node_name),
        api_addr,
        reporting_addr,
        shutdown.clone(),
    );

    // A maintainer keeps `council_rx` pointing at the current leader's
    // reporting address (the worker reports there) and `epoch_rx` at the
    // current leadership term (the aggregator scopes reports to it).
    let (council_tx, council_rx) = watch::channel::<Vec<(NodeId, SocketAddr)>>(Vec::new());
    let (epoch_tx, epoch_rx) = watch::channel(0u64);
    spawn_leader_target_maintainer(
        raft_metrics_rx.clone(),
        directory_rx.clone(),
        council_tx,
        epoch_tx,
        api_offset,
        reporting_offset,
        shutdown.clone(),
    );

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
        Some(epoch_rx),
        Some(membership_rx.clone()),
    );
    spawn_supervised("report aggregator", shutdown.clone(), async move {
        aggregator.run().await
    });

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
    spawn_supervised("report worker", shutdown.clone(), async move {
        worker.run().await
    });

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
        spawn_supervised("rollup worker", shutdown.clone(), async move {
            rollup_worker.run().await
        });
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
            directory_rx,
        },
    ))
}

/// Spawn a long-lived subsystem task and log loudly if it exits while the
/// node is still running.
///
/// Every task passed here is wired to stop only at shutdown; an earlier
/// exit means a closed channel or a dead transport — a bug we want visible
/// in the logs, not a silently missing subsystem (CP10). We deliberately
/// don't respawn: each of these tasks owns sockets and channel endpoints
/// that can't be rebuilt from inside a generic wrapper, and a
/// degraded-but-alive node still serves traffic, so the honest move is to
/// surface the failure and keep the rest of the node up.
fn spawn_supervised(
    name: &'static str,
    shutdown: CancellationToken,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        task.await;
        if !shutdown.is_cancelled() {
            eprintln!("cluster: {name} task exited unexpectedly (before shutdown)");
        }
    });
}

/// Publish a [`LeaderHint`] naming THIS node while it is the Raft leader,
/// and `None` otherwise. The gossip node stamps the hint (or the best
/// relayed one) onto every outgoing datagram; term ordering lets the whole
/// cluster converge on the newest leader.
fn spawn_leader_hint_publisher(
    mut metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    hint_tx: watch::Sender<Option<LeaderHint>>,
    self_id: u64,
    node_id: NodeId,
    api_address: SocketAddr,
    reporting_address: SocketAddr,
    shutdown: CancellationToken,
) {
    spawn_supervised("leader hint publisher", shutdown.clone(), async move {
        loop {
            let hint = {
                let m = metrics_rx.borrow();
                (m.current_leader == Some(self_id)).then(|| LeaderHint {
                    node_id: node_id.clone(),
                    term: m.current_term,
                    api_address,
                    reporting_address,
                })
            };
            hint_tx.send_if_modified(|current| {
                if *current != hint {
                    *current = hint;
                    true
                } else {
                    false
                }
            });
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

/// Keep `council_tx` pointing at the current Raft leader's reporting address
/// (a single-element council list), so the flat-star worker always reports
/// to the leader, and `epoch_tx` at the leadership term the aggregator
/// scopes reports to.
///
/// Resolution combines local Raft metrics (authoritative on voters) with
/// the gossip directory (the only source on workers outside the council),
/// via [`crate::cluster::directory::resolve_leader`]. This is the H1 fix:
/// the old version read metrics alone, so an eighth node in a seven-voter
/// council never found anywhere to report.
fn spawn_leader_target_maintainer(
    mut metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    mut directory_rx: watch::Receiver<NodeDirectory>,
    council_tx: watch::Sender<Vec<(NodeId, SocketAddr)>>,
    epoch_tx: watch::Sender<u64>,
    api_offset: i32,
    reporting_offset: i32,
    shutdown: CancellationToken,
) {
    spawn_supervised("leader target maintainer", shutdown.clone(), async move {
        loop {
            let view = {
                let metrics = metrics_rx.borrow();
                let directory = directory_rx.borrow();
                crate::cluster::directory::resolve_leader(
                    &metrics,
                    &directory,
                    api_offset,
                    reporting_offset,
                )
            };
            if let Some(view) = view {
                if let Some(reporting) = view.reporting_address {
                    let target = vec![(view.node_id.clone(), reporting)];
                    council_tx.send_if_modified(|current| {
                        if *current != target {
                            *current = target;
                            true
                        } else {
                            false
                        }
                    });
                }
                // Terms only grow; ignoring lower values keeps a lagging
                // gossip hint from briefly rolling the epoch back.
                epoch_tx.send_if_modified(|epoch| {
                    if view.term > *epoch {
                        *epoch = view.term;
                        true
                    } else {
                        false
                    }
                });
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = metrics_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = directory_rx.changed() => {
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

/// One gossiped node as the reconciler sees it: its gossip identity, SWIM
/// observation and derived Raft connection info, keyed by Raft id in
/// [`council_directory`].
#[derive(Debug, Clone)]
struct DirectoryEntry {
    node_id: NodeId,
    member: ObservedMember,
    info: CouncilNodeInfo,
}

/// Map every gossiped member (plus self, always alive) to its Raft id.
///
/// This is the one place the reconciler translates gossip names into Raft
/// ids; everything downstream — health tracking, candidate ranking, the
/// planner — works on `u64`s from this directory.
fn council_directory(
    snapshot: &[MembershipSnapshot],
    self_id: u64,
    self_info: &CouncilNodeInfo,
    port_offset: i32,
    now: Instant,
) -> HashMap<u64, DirectoryEntry> {
    let mut directory = HashMap::with_capacity(snapshot.len() + 1);
    directory.insert(
        self_id,
        DirectoryEntry {
            node_id: NodeId::new(self_info.name.clone()),
            member: ObservedMember {
                state: NodeState::Alive,
                first_seen: now,
            },
            info: self_info.clone(),
        },
    );
    for member in snapshot {
        let (rid, info) = identity::council_info(member, port_offset);
        directory.entry(rid).or_insert_with(|| DirectoryEntry {
            node_id: member.node_id.clone(),
            member: ObservedMember {
                state: member.state,
                first_seen: member.first_seen,
            },
            info,
        });
    }
    directory
}

/// Timing knobs for the council reconciler. The defaults are production
/// values; the gated cluster tests shrink them to sub-second windows.
#[derive(Debug, Clone)]
pub struct CouncilReconcilerConfig {
    /// Selection thresholds and self-healing hysteresis windows.
    pub selection: CouncilSelectionConfig,
    /// How often to reconcile the council against gossip membership.
    pub tick_interval: Duration,
    /// Upper bound on a single membership operation. A peer whose Raft port
    /// is unreachable must not block the reconciler forever — on timeout we
    /// log and retry on the next tick.
    pub op_timeout: Duration,
}

impl Default for CouncilReconcilerConfig {
    fn default() -> Self {
        Self {
            // min_node_age is 0 so a fresh cluster can form a council
            // immediately; age still affects *ordering* (older nodes
            // preferred), just not eligibility.
            selection: CouncilSelectionConfig {
                min_node_age: Duration::from_secs(0),
                ..Default::default()
            },
            tick_interval: RECONCILE_INTERVAL,
            op_timeout: RECONCILE_OP_TIMEOUT,
        }
    }
}

/// Spawn the council reconciler with production timing. See
/// [`spawn_council_reconciler_with_config`].
pub fn spawn_council_reconciler(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    port_offset: i32,
    self_id: u64,
    self_info: CouncilNodeInfo,
    shutdown: CancellationToken,
) {
    spawn_council_reconciler_with_config(
        council,
        membership_rx,
        port_offset,
        self_id,
        self_info,
        CouncilReconcilerConfig::default(),
        shutdown,
    );
}

/// Spawn the council reconciler: on the leader, each tick re-plans the Raft
/// membership from observed state (gossip health, Raft metrics) and executes
/// at most one action — add a learner, promote a caught-up learner, evict a
/// dead voter, or drop a dead learner. No-ops on followers, so it's safe to
/// run on every node; followers still feed the health tracker so a freshly
/// elected leader doesn't start blind.
///
/// Planning is idempotent (nothing is assumed about the previous tick's
/// action landing), every operation is bounded by `op_timeout`, and errors
/// are logged rather than propagated — the non-wedging property from M15.
pub fn spawn_council_reconciler_with_config(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    port_offset: i32,
    self_id: u64,
    self_info: CouncilNodeInfo,
    config: CouncilReconcilerConfig,
    shutdown: CancellationToken,
) {
    // No disk-pressure signal: the reconciler behaves exactly as before this
    // theme. Dropping the sender is fine — `watch::Receiver::borrow` keeps
    // returning the initial empty set after the sender is gone.
    let (_pressure_tx, pressure_rx) = watch::channel(BTreeSet::new());
    spawn_council_reconciler_with_pressure(
        council,
        membership_rx,
        pressure_rx,
        port_offset,
        self_id,
        self_info,
        config,
        shutdown,
    );
}

/// As [`spawn_council_reconciler_with_config`], plus a `disk_pressured_rx`
/// carrying the Raft ids of voters (including possibly this node) that have
/// resigned under sustained disk pressure (12b.2 T3). The leader feeds this
/// set to the planner so a pressured follower is replaced add-before-remove,
/// and if this node is the pressured leader it deposes itself first.
#[allow(clippy::too_many_arguments)]
pub fn spawn_council_reconciler_with_pressure(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    disk_pressured_rx: watch::Receiver<BTreeSet<u64>>,
    port_offset: i32,
    self_id: u64,
    self_info: CouncilNodeInfo,
    config: CouncilReconcilerConfig,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tracker = HealthTracker::default();
        let mut tick = tokio::time::interval(config.tick_interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let disk_pressured = disk_pressured_rx.borrow().clone();
                    reconcile_council_once(
                        &council,
                        &membership_rx,
                        port_offset,
                        self_id,
                        &self_info,
                        &config,
                        &mut tracker,
                        &disk_pressured,
                    )
                    .await;
                }
            }
        }
    });
}

/// One reconciler tick: observe, plan, execute at most one action.
#[allow(clippy::too_many_arguments)]
async fn reconcile_council_once(
    council: &CouncilNode,
    membership_rx: &watch::Receiver<Vec<MembershipSnapshot>>,
    port_offset: i32,
    self_id: u64,
    self_info: &CouncilNodeInfo,
    config: &CouncilReconcilerConfig,
    tracker: &mut HealthTracker,
    disk_pressured: &BTreeSet<u64>,
) {
    let now = Instant::now();
    let snapshot = membership_rx.borrow().clone();
    let directory = council_directory(&snapshot, self_id, self_info, port_offset, now);

    let metrics = council.metrics().borrow().clone();
    let membership = metrics.membership_config.membership().clone();
    let voters: BTreeSet<u64> = membership.voter_ids().collect();
    let learners: BTreeSet<u64> = membership
        .nodes()
        .map(|(id, _)| *id)
        .filter(|id| !voters.contains(id))
        .collect();
    let change_in_flight = membership.get_joint_config().len() > 1;

    // Rank spares against the voters gossip can still see; a reaped dead
    // voter drops out of `current_council` here, which is exactly what
    // frees its seat for the best-ranked replacement.
    let present_voters: Vec<NodeId> = voters
        .iter()
        .filter_map(|id| directory.get(id).map(|e| e.node_id.clone()))
        .collect();
    let selected = select_council_candidates(
        &snapshot,
        &present_voters,
        config.selection.max_council_size,
        &config.selection,
        now,
    );
    let candidates: Vec<u64> = selected
        .iter()
        .map(|nid| identity::raft_id_from_name(&nid.0))
        .collect();

    // Track health for everyone the planner may reason about. This runs on
    // followers too, so the hysteresis clocks are warm on leader failover.
    let tracked: BTreeSet<u64> = voters
        .iter()
        .chain(learners.iter())
        .chain(candidates.iter())
        .copied()
        .collect();
    let present: HashMap<u64, ObservedMember> = directory
        .iter()
        .map(|(id, entry)| (*id, entry.member))
        .collect();
    let health = tracker.observe(&present, &tracked, now);

    // Leader disk-pressure resignation (12b.2 T3). openraft 0.9 has no
    // graceful leadership transfer, and a node can only trigger an election on
    // *itself*. So the pressured leader can't hand off directly; instead the
    // chosen healthy follower campaigns. A follower's election at a higher term
    // deposes the pressured leader by Raft's own rules, after which the ex-
    // leader is an ordinary voter the new leader's planner replaces. Runs on
    // followers (before the leader gate) so exactly the deposition target acts.
    let current_leader = metrics.current_leader;
    if let Some(leader) = current_leader
        && leader != self_id
        && disk_pressured.contains(&leader)
        && pick_deposition_target(&voters, &health, leader) == Some(self_id)
    {
        match council.raft().trigger().elect().await {
            Ok(()) => eprintln!(
                "council reconciler: voter {self_id} campaigning to depose disk-pressured leader {leader}"
            ),
            Err(e) => eprintln!(
                "council reconciler: voter {self_id} failed to trigger deposition election: {e}"
            ),
        }
        return;
    }

    if !council.is_leader().await {
        return;
    }

    // A pressured leader never plans its own removal; it waits for a follower
    // to depose it (above), then gets replaced as an ordinary voter.
    if disk_pressured.contains(&self_id) {
        if pick_deposition_target(&voters, &health, self_id).is_none() {
            eprintln!(
                "council reconciler: leader {self_id} under disk pressure but no healthy voter to hand off to; holding"
            );
        }
        return;
    }

    let replication: BTreeMap<u64, Option<u64>> = metrics
        .replication
        .as_ref()
        .map(|r| {
            r.iter()
                .map(|(id, log_id)| (*id, log_id.map(|l| l.index)))
                .collect()
        })
        .unwrap_or_default();

    let observation = CouncilObservation {
        voters: &voters,
        learners: &learners,
        leader_id: self_id,
        health: &health,
        candidates: &candidates,
        leader_last_log_index: metrics.last_log_index,
        replication: &replication,
        change_in_flight,
        disk_pressured,
    };
    let action = plan_council_action(&observation, &config.selection, now);
    execute_council_action(council, action, &voters, &directory, config.op_timeout).await;
}

/// Pick a healthy voter (not the leader itself) for the leader to hand off to
/// when resigning under disk pressure. Prefers a stably-alive voter; returns
/// `None` when the leader is the only viable voter, in which case resignation
/// waits rather than risking an election no one can win.
fn pick_deposition_target(
    voters: &BTreeSet<u64>,
    health: &HashMap<u64, crate::council::selection::MemberHealth>,
    leader_id: u64,
) -> Option<u64> {
    use crate::council::selection::MemberHealth;
    voters
        .iter()
        .copied()
        .find(|id| *id != leader_id && matches!(health.get(id), Some(MemberHealth::Alive { .. })))
}

/// Execute a single planner action against the council, bounded by
/// `op_timeout`. Failures are logged, never propagated: the next tick
/// re-plans from observed state, so retrying is free and safe.
async fn execute_council_action(
    council: &CouncilNode,
    action: CouncilAction,
    voters: &BTreeSet<u64>,
    directory: &HashMap<u64, DirectoryEntry>,
    op_timeout: Duration,
) {
    match action {
        CouncilAction::Nothing => {}
        CouncilAction::AddLearner(id) => {
            // Gossip may have moved on since the plan; the next tick will
            // re-plan, so a vanished candidate is simply skipped.
            let Some(entry) = directory.get(&id) else {
                return;
            };
            let result =
                tokio::time::timeout(op_timeout, council.add_learner(id, entry.info.clone())).await;
            log_membership_op(&format!("add_learner({id})"), result);
        }
        CouncilAction::Promote(id) => {
            let mut next = voters.clone();
            next.insert(id);
            let result = tokio::time::timeout(op_timeout, council.change_membership(next)).await;
            log_membership_op(&format!("promote({id})"), result);
        }
        CouncilAction::RemoveVoter(id) => {
            let mut next = voters.clone();
            next.remove(&id);
            let result =
                tokio::time::timeout(op_timeout, council.change_membership_evicting(next)).await;
            log_membership_op(&format!("remove_voter({id})"), result);
        }
        CouncilAction::RemoveLearner(id) => {
            let result = tokio::time::timeout(op_timeout, council.remove_learner(id)).await;
            log_membership_op(&format!("remove_learner({id})"), result);
        }
    }
}

/// Log the outcome of a bounded membership operation without discarding
/// the error — an unreachable peer must not wedge the reconciler.
fn log_membership_op(
    operation: &str,
    result: Result<Result<(), crate::council::CouncilError>, tokio::time::error::Elapsed>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("council reconciler: {operation} failed: {e}"),
        Err(_) => eprintln!("council reconciler: {operation} timed out; will retry"),
    }
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
    fn directory_maps_self_and_gossip_members() {
        let now = Instant::now();
        let self_info = info("leader", 9444);
        let self_id = raft_id_from_name("leader");
        let snapshot = vec![snap("peer-a", 9445, now)];

        let directory = council_directory(&snapshot, self_id, &self_info, 1, now);

        // Self is present and always observed alive, whatever gossip says.
        let own = directory.get(&self_id).unwrap();
        assert_eq!(own.member.state, NodeState::Alive);
        assert_eq!(own.info.name, "leader");

        // The peer's Raft id and address derive from its gossip identity.
        let peer = directory.get(&raft_id_from_name("peer-a")).unwrap();
        assert_eq!(peer.node_id, NodeId::new("peer-a"));
        assert_eq!(peer.info.addr, "127.0.0.1:9446".parse().unwrap());
    }

    /// Reconciler config with zero hysteresis so in-memory growth tests run
    /// fast; the windows themselves are covered by the planner unit tests.
    fn fast_reconciler_config() -> CouncilReconcilerConfig {
        CouncilReconcilerConfig {
            selection: CouncilSelectionConfig {
                min_node_age: Duration::from_secs(0),
                dead_window: Duration::from_secs(0),
                candidate_alive_window: Duration::from_secs(0),
                ..Default::default()
            },
            tick_interval: Duration::from_millis(50),
            op_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn reconciler_ticks_grow_council_one_action_at_a_time() {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};

        let router = InMemoryRaftRouter::new();
        let names = ["node-1", "node-2", "node-3"];
        let mut nodes = Vec::new();
        for name in names {
            let id = raft_id_from_name(name);
            let network = InMemoryRaftNetworkFactory::new(id, router.clone());
            let node = CouncilNode::new(
                id,
                crate::council::types::CouncilConfig {
                    heartbeat_interval_ms: 50,
                    election_timeout_min_ms: 200,
                    election_timeout_max_ms: 400,
                    snapshot_threshold: 100,
                    max_in_snapshot_log_to_keep: 50,
                },
                network,
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap();
            router.register(id, node.raft().clone()).await;
            nodes.push(node);
        }

        // Bootstrap node-1 alone; it becomes leader of a 1-voter council.
        let self_id = raft_id_from_name("node-1");
        let self_info = info("node-1", 9444);
        let mut members = BTreeMap::new();
        members.insert(self_id, self_info.clone());
        nodes[0].initialize(members).await.unwrap();

        // Gossip sees the two peers, alive and warm.
        let now = Instant::now();
        let mut peer_a = snap("node-2", 9445, now);
        peer_a.first_seen = now - Duration::from_secs(600);
        let mut peer_b = snap("node-3", 9447, now);
        peer_b.first_seen = now - Duration::from_secs(600);
        let (_membership_tx, membership_rx) = watch::channel(vec![peer_a, peer_b]);

        // Drive ticks manually until the council reaches three voters. Each
        // tick performs at most one membership action.
        let config = fast_reconciler_config();
        let mut tracker = HealthTracker::default();
        let no_pressure = BTreeSet::new();
        let expected: BTreeSet<u64> = names.iter().map(|n| raft_id_from_name(n)).collect();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            reconcile_council_once(
                &nodes[0],
                &membership_rx,
                1,
                self_id,
                &self_info,
                &config,
                &mut tracker,
                &no_pressure,
            )
            .await;

            let voters: BTreeSet<u64> = nodes[0]
                .metrics()
                .borrow()
                .membership_config
                .membership()
                .voter_ids()
                .collect();
            // The leader keeps its seat through every intermediate config.
            assert!(voters.contains(&self_id), "leader dropped from {voters:?}");
            if voters == expected {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "council did not grow to 3 voters; got {voters:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

    #[tokio::test]
    async fn reconciler_tick_noop_with_no_candidates() {
        let council = single_bootstrap_council().await;
        let self_id = 1u64;
        let self_info = info("node-1", 9444);
        let (_membership_tx, membership_rx) = watch::channel(Vec::new());

        let config = fast_reconciler_config();
        let mut tracker = HealthTracker::default();
        // A couple of ticks with an empty gossip view: the single-voter
        // council must stay exactly as it is.
        let no_pressure = BTreeSet::new();
        for _ in 0..3 {
            reconcile_council_once(
                &council,
                &membership_rx,
                1,
                self_id,
                &self_info,
                &config,
                &mut tracker,
                &no_pressure,
            )
            .await;
        }

        let voters: BTreeSet<u64> = council
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        assert_eq!(voters, BTreeSet::from([1]));
    }
}

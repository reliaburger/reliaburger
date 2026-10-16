//! Measure how long a council leader takes to confirm its leadership (#928).
//!
//! Forms a three-voter council over the production transport (TCP, mTLS with
//! live node identities and node-id-bound connectors, as Bun dials it) on
//! loopback, then times `ensure_linearizable` on the leader. That call is what
//! every linearizable read pays: openraft sends one heartbeat to each voter
//! and waits for a quorum, giving each heartbeat `heartbeat_interval`
//! (150 ms) to come back.
//!
//! Run it in both profiles, idle and under CPU load:
//!
//! ```text
//! cargo run --example raft-heartbeat-rtt -- --samples 1000
//! cargo run --release --example raft-heartbeat-rtt -- --samples 1000
//! ```
//!
//! The results are recorded in `docs/design/gossip-mustard.md`, section 3.3.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use reliaburger::council::fence::RecoveryFence;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{RaftTlsMaterial, TcpRaftNetworkFactory, serve_raft_rpc};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::credentials::LiveNodeIdentity;
use reliaburger::sesame::identity_store::{self, NodeIdentity};
use reliaburger::sesame::mtls::{CrlHandle, build_live_mtls_server_config};
use reliaburger::sesame::types::SerialNumber;

#[derive(Parser)]
struct Options {
    /// How many leadership confirmations to time.
    #[arg(long, default_value_t = 1000)]
    samples: usize,
    /// Pause between confirmations, in milliseconds.
    #[arg(long, default_value_t = 5)]
    gap_ms: u64,
}

/// openraft's per-heartbeat budget for a leadership check.
const HEARTBEAT_BUDGET: Duration = Duration::from_millis(150);

fn identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> anyhow::Result<NodeIdentity> {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node_id,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )?;
    let now = SystemTime::now();
    Ok(NodeIdentity {
        node_id: node_id.to_string(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        trust: reliaburger::sesame::trust::TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ),
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now - Duration::from_secs(60),
        not_after: now + Duration::from_secs(24 * 3600),
    })
}

struct Node {
    council: Arc<CouncilNode>,
    address: SocketAddr,
    _directory: tempfile::TempDir,
}

async fn start_node(
    id: u64,
    hierarchy: &CaHierarchy,
    shutdown: &CancellationToken,
) -> anyhow::Result<Node> {
    let directory = tempfile::tempdir()?;
    identity_store::save(directory.path(), &identity(hierarchy, &format!("n{id}"), 10 + id)?)?;
    let live = LiveNodeIdentity::load(directory.path())?;
    let crl = CrlHandle::default();
    let acceptor = tokio_rustls::TlsAcceptor::from(build_live_mtls_server_config(&live, crl.clone())?);
    let factory = TcpRaftNetworkFactory::new_tls_bound(id, RaftTlsMaterial::new(live, crl));
    let council = Arc::new(
        CouncilNode::new(
            id,
            CouncilConfig::default(),
            factory,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await?,
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let raft = council.raft().clone();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        serve_raft_rpc(listener, raft, serving, Some(acceptor), RecoveryFence::serving(0)).await;
    });
    Ok(Node {
        council,
        address,
        _directory: directory,
    })
}

fn info(id: u64, address: SocketAddr) -> CouncilNodeInfo {
    CouncilNodeInfo {
        addr: address,
        name: format!("n{id}"),
    }
}

async fn form_council(nodes: &[Node]) -> anyhow::Result<()> {
    let leader = &nodes[0].council;
    leader
        .initialize(BTreeMap::from([(1, info(1, nodes[0].address))]))
        .await?;
    for _ in 0..100 {
        if leader.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for (index, node) in nodes.iter().enumerate().skip(1) {
        let id = index as u64 + 1;
        leader.add_learner(id, info(id, node.address)).await?;
    }
    let voters: BTreeSet<u64> = (1..=nodes.len() as u64).collect();
    leader.change_membership(voters).await?;
    Ok(())
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    if options.samples == 0 {
        bail!("--samples must be at least 1");
    }
    let hierarchy = ca::generate_ca_hierarchy("raft-heartbeat-rtt", b"ikm")?;
    let shutdown = CancellationToken::new();
    let mut nodes = Vec::new();
    for id in 1..=3 {
        nodes.push(start_node(id, &hierarchy, &shutdown).await?);
    }
    form_council(&nodes).await.context("forming the council")?;

    let leader = nodes[0].council.raft();
    // Let replication settle before timing anything.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut samples = Vec::with_capacity(options.samples);
    let mut failures = 0usize;
    for _ in 0..options.samples {
        let started = Instant::now();
        match leader.ensure_linearizable().await {
            Ok(_) => samples.push(started.elapsed()),
            Err(_) => failures += 1,
        }
        tokio::time::sleep(Duration::from_millis(options.gap_ms)).await;
    }
    samples.sort();
    let over_budget = samples.iter().filter(|s| **s > HEARTBEAT_BUDGET).count();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if samples.is_empty() {
        println!("profile={profile} samples=0 failures={failures}");
    } else {
        println!(
            "profile={profile} samples={} failures={failures} over_150ms={over_budget} \
             p50={:.2?} p90={:.2?} p99={:.2?} max={:.2?}",
            samples.len(),
            percentile(&samples, 0.50),
            percentile(&samples, 0.90),
            percentile(&samples, 0.99),
            percentile(&samples, 1.0),
        );
    }

    shutdown.cancel();
    for node in nodes {
        node.council.shutdown().await.ok();
    }
    Ok(())
}

//! A recovered survivor re-forms its council even with join seeds (#429).
use reliaburger::cluster::runtime::{ClusterParams, start};
use reliaburger::council::types::DesiredState;
use std::path::Path;
use std::time::Duration;

/// Start a node on `data_dir` whose config lists an unreachable join seed,
/// and report (voters, leader) after `settle`.
async fn start_with_a_seed(data_dir: &Path, settle: Duration) -> (usize, Option<u64>) {
    let cancel = tokio_util::sync::CancellationToken::new();
    // Ports from the suite's reserved range, each free for TCP and UDP. A
    // `bind(0)` port checked only over TCP was taken for the gossip UDP
    // socket before the node bound it ("Address already in use").
    let base = crate::bun_process::reserve_port_block(4);
    let (handle, _runtime) = start(
        ClusterParams {
            node_name: "survivor".into(),
            gossip_addr: std::net::SocketAddr::from(([127, 0, 0, 1], base)),
            raft_port: base + 1,
            reporting_port: base + 2,
            api_port: base + 3,
            reporting_config: Default::default(),
            seeds: vec!["127.0.0.1:9".parse().unwrap()],
            wrapping_ikm: None,
            bootstrap_security_state: None,
            bootstrap_council_size: None,
            data_dir: data_dir.into(),
            mayo: None,
            rollup_interval: Duration::from_secs(60),
            identity: None,
            backup: Default::default(),
            labels: Default::default(),
            self_disk_pressured_rx: None,
            readiness: None,
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    let council = handle.council.as_ref().unwrap();
    let mut metrics = council.metrics();
    let _ = tokio::time::timeout(settle, metrics.wait_for(|m| m.current_leader.is_some())).await;
    let voters = council
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .count();
    let leader = council.current_leader().await;
    cancel.cancel();
    council.shutdown().await.unwrap();
    (voters, leader)
}

#[tokio::test]
async fn a_recovered_node_with_configured_seeds_must_bootstrap() {
    let root = tempfile::tempdir().unwrap();
    reliaburger::council::recovery::recover_data_dir(root.path(), DesiredState::default()).unwrap();
    let (voters, leader) = start_with_a_seed(root.path(), Duration::from_secs(10)).await;
    assert!(
        voters == 1 && leader.is_some(),
        "a seeded survivor never initialised its recovered council: voters={voters}, leader={leader:?}"
    );
}

#[tokio::test]
async fn a_fresh_node_with_seeds_waits_for_its_council_instead_of_bootstrapping() {
    let root = tempfile::tempdir().unwrap();
    let (voters, leader) = start_with_a_seed(root.path(), Duration::from_millis(1500)).await;
    assert_eq!(
        (voters, leader),
        (0, None),
        "a fresh joiner must never bootstrap a council of its own"
    );
}

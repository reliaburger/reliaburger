//! A recovered survivor re-forms its council even with join seeds (#429).
use reliaburger::cluster::runtime::{ClusterParams, start};
use reliaburger::council::types::DesiredState;
use std::path::Path;
use std::time::Duration;

fn free() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Start a node on `data_dir` whose config lists an unreachable join seed,
/// and report (voters, leader) after `settle`.
async fn start_with_a_seed(data_dir: &Path, settle: Duration) -> (usize, Option<u64>) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let (handle, _runtime) = start(
        ClusterParams {
            node_name: "survivor".into(),
            gossip_addr: free(),
            raft_port: free().port(),
            reporting_port: free().port(),
            api_port: free().port(),
            reporting_config: Default::default(),
            seeds: vec!["127.0.0.1:9".parse().unwrap()],
            wrapping_ikm: None,
            bootstrap_security_state: None,
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

use reliaburger::config::Replicas;
use reliaburger::config::app::AppSpec;
use reliaburger::council::types::{DesiredState, RaftRequest};
use reliaburger::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};
use reliaburger::meat::quota::QuotaLedger;
use reliaburger::meat::types::{AppId, NodeId, Placement, Resources};
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::aggregator::AggregatedState;
use reliaburger::reporting::types::*;
use reliaburger::{bun, cluster, config, council, meat, mustard};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};
#[tokio::test]
async fn a_recovered_node_with_configured_seeds_must_bootstrap() {
    use cluster::runtime::{ClusterParams, start};
    let root = tempfile::tempdir().unwrap();
    council::recovery::recover_data_dir(root.path(), DesiredState::default()).unwrap();
    let free = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    };
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
            data_dir: root.path().into(),
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
    tokio::time::sleep(Duration::from_millis(800)).await;
    let voters = council
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .count();
    let leader = council.current_leader().await;
    let epoch = council.desired_state().await.recovery_epoch;
    cancel.cancel();
    council.shutdown().await.unwrap();
    eprintln!("recovered node: epoch={epoch}, voter_count={voters}, leader={leader:?}");
    assert!(
        voters == 1 && leader.is_some(),
        "normal seeded survivor never initializes its new council"
    );
}

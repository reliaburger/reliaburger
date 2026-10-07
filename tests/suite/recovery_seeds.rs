//! A recovered survivor re-forms its council even with join seeds (#429),
//! and keeps the security state it restored (#477).
use reliaburger::cluster::runtime::{ClusterParams, start};
use reliaburger::council::types::DesiredState;
use reliaburger::sesame::types::SecurityState;
use std::path::Path;
use std::time::Duration;

/// What a started node reported before it was shut down again.
struct Started {
    voters: usize,
    leader: Option<u64>,
    security: SecurityState,
}

/// Start a node on `data_dir` whose config lists an unreachable join seed
/// and, optionally, a security bootstrap file, and report what it holds
/// after `settle`.
async fn start_with_a_seed(
    data_dir: &Path,
    settle: Duration,
    bootstrap_security_state: Option<Box<SecurityState>>,
) -> Started {
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
            bootstrap_security_state,
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
    let security = council.security_state().await;
    cancel.cancel();
    council.shutdown().await.unwrap();
    Started {
        voters,
        leader,
        security,
    }
}

#[tokio::test]
async fn a_recovered_node_with_configured_seeds_must_bootstrap() {
    let root = tempfile::tempdir().unwrap();
    reliaburger::council::recovery::recover_data_dir(root.path(), DesiredState::default()).unwrap();
    let started = start_with_a_seed(root.path(), Duration::from_secs(10), None).await;
    assert!(
        started.voters == 1 && started.leader.is_some(),
        "a seeded survivor never initialised its recovered council: voters={}, leader={:?}",
        started.voters,
        started.leader
    );
}

#[tokio::test]
async fn a_fresh_node_with_seeds_waits_for_its_council_instead_of_bootstrapping() {
    let root = tempfile::tempdir().unwrap();
    let started = start_with_a_seed(root.path(), Duration::from_millis(1500), None).await;
    assert_eq!(
        (started.voters, started.leader),
        (0, None),
        "a fresh joiner must never bootstrap a council of its own"
    );
}

/// #477: the recovered snapshot holds the cluster's API tokens. The node's
/// bootstrap file (`security.bootstrap_path`) is the init-time state: the
/// same CAs, no tokens. Re-committing it on the recovered council used to
/// wipe every token, so the API listener refused to start.
#[tokio::test]
async fn a_recovered_council_keeps_its_restored_tokens_over_the_bootstrap_file() {
    let hierarchy = reliaburger::sesame::ca::generate_ca_hierarchy("restored", &[5; 32]).unwrap();
    let token = reliaburger::sesame::types::ApiToken {
        name: "admin".into(),
        token_hash: vec![1],
        token_salt: vec![2],
        role: reliaburger::sesame::types::ApiRole::Admin,
        scope: Default::default(),
        expires_at: None,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        previous_secret: None,
    };
    let restored = DesiredState {
        security_state: SecurityState {
            certificate_authorities: vec![hierarchy.root.ca.clone()],
            api_tokens: vec![token],
            next_serial: 900,
            ..Default::default()
        },
        ..Default::default()
    };
    let bootstrap = SecurityState {
        certificate_authorities: vec![hierarchy.root.ca],
        next_serial: 2,
        ..Default::default()
    };
    let root = tempfile::tempdir().unwrap();
    reliaburger::council::recovery::recover_data_dir(root.path(), restored).unwrap();
    let started = start_with_a_seed(
        root.path(),
        Duration::from_secs(10),
        Some(Box::new(bootstrap)),
    )
    .await;
    assert!(started.leader.is_some(), "the recovered council never led");
    let names: Vec<_> = started
        .security
        .api_tokens
        .iter()
        .map(|token| token.name.as_str())
        .collect();
    assert_eq!(names, ["admin"], "the restored API tokens were replaced");
    assert_eq!(started.security.next_serial, 900);
}

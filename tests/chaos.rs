/// In-memory chaos tests for CI.
///
/// Tests cluster recovery from network partitions using in-memory
/// transports. Fast and deterministic.
use std::collections::BTreeMap;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};

fn fast_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

fn node_info(id: u64) -> CouncilNodeInfo {
    CouncilNodeInfo::new(
        format!("127.0.0.1:{}", 9000 + id).parse().unwrap(),
        format!("node-{id}"),
    )
}

async fn create_cluster(n: u64) -> (Vec<CouncilNode>, InMemoryRaftRouter) {
    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for id in 1..=n {
        let network = InMemoryRaftNetworkFactory::new(id, router.clone());
        let node = CouncilNode::new(
            id,
            fast_config(),
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
    (nodes, router)
}

async fn init_cluster(nodes: &[CouncilNode]) {
    let mut members = BTreeMap::new();
    for (i, _) in nodes.iter().enumerate() {
        let id = (i + 1) as u64;
        members.insert(id, node_info(id));
    }
    nodes[0].initialize(members).await.unwrap();
}

async fn wait_for_leader(nodes: &[CouncilNode], timeout: Duration) -> Option<u64> {
    let start = tokio::time::Instant::now();
    loop {
        for node in nodes {
            if let Some(leader) = node.current_leader().await {
                return Some(leader);
            }
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Council partition 3/2: majority continues, minority can't write, heal converges.
#[tokio::test]
async fn chaos_council_partition_majority_continues() {
    let (nodes, router) = create_cluster(5).await;
    init_cluster(&nodes).await;

    let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
        .await
        .expect("leader should be elected");

    let leader = &nodes[(leader_id - 1) as usize];

    // Write before partition
    leader
        .write(RaftRequest::ConfigSet {
            key: "before".to_string(),
            value: "partition".to_string(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Partition: isolate nodes 4,5 from 1,2,3
    let minority = [4u64, 5];
    let majority = [1u64, 2, 3];
    for &m in &minority {
        for &j in &majority {
            router.partition(m, j).await;
        }
    }

    // Wait for majority to stabilise
    let majority_nodes: Vec<&CouncilNode> = majority
        .iter()
        .map(|&id| &nodes[(id - 1) as usize])
        .collect();

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Majority should have a leader
    let mut majority_leader = None;
    for node in &majority_nodes {
        if let Some(lid) = node.current_leader().await
            && majority.contains(&lid)
        {
            majority_leader = Some(lid);
            break;
        }
    }
    assert!(majority_leader.is_some(), "majority should have a leader");

    // Majority can write
    let ml = &nodes[(majority_leader.unwrap() - 1) as usize];
    ml.write(RaftRequest::ConfigSet {
        key: "during".to_string(),
        value: "partition".to_string(),
    })
    .await
    .unwrap();

    // Minority should NOT have the post-partition write
    let minority_state = nodes[3].desired_state().await;
    assert!(
        !minority_state.config.contains_key("during"),
        "minority should not see writes during partition"
    );

    // Heal
    router.heal().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // All nodes should converge
    for (i, node) in nodes.iter().enumerate() {
        let state = node.desired_state().await;
        assert_eq!(
            state.config.get("before").map(String::as_str),
            Some("partition"),
            "node {} missing 'before' key after heal",
            i + 1
        );
        assert_eq!(
            state.config.get("during").map(String::as_str),
            Some("partition"),
            "node {} missing 'during' key after heal",
            i + 1
        );
    }
}

/// Isolate one council member: the majority keeps making progress and
/// the isolated node genuinely misses writes until it is healed.
///
/// The previous version of this test never actually partitioned anything
/// — it asserted that all nodes saw a write nobody was prevented from
/// receiving. This version cuts node 3 off from nodes 1 and 2 through the
/// router, so the assertions describe a real partition: the majority
/// writes, the isolated node does NOT see that write, and healing lets it
/// catch up.
#[tokio::test]
async fn chaos_isolated_member_misses_writes_until_healed() {
    let (nodes, router) = create_cluster(3).await;
    init_cluster(&nodes).await;

    wait_for_leader(&nodes, Duration::from_secs(5))
        .await
        .expect("leader should be elected");

    // Cut node 3 off from the majority (1, 2) in both directions.
    let isolated = 3u64;
    let majority = [1u64, 2];
    for &m in &majority {
        router.partition(isolated, m).await;
    }

    // The majority must re-establish (or keep) a leader among {1, 2}.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut majority_leader = None;
    for &id in &majority {
        if let Some(lid) = nodes[(id - 1) as usize].current_leader().await
            && majority.contains(&lid)
        {
            majority_leader = Some(lid);
            break;
        }
    }
    let majority_leader = majority_leader.expect("majority {1,2} should hold a leader");

    // The majority leader writes while node 3 is isolated.
    nodes[(majority_leader - 1) as usize]
        .write(RaftRequest::ConfigSet {
            key: "during_isolation".to_string(),
            value: "committed".to_string(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The isolated node must NOT have the write — proof the cut is real.
    let isolated_state = nodes[(isolated - 1) as usize].desired_state().await;
    assert!(
        !isolated_state.config.contains_key("during_isolation"),
        "isolated node should not receive writes while partitioned"
    );

    // Heal and let node 3 catch up.
    router.heal().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    for (i, node) in nodes.iter().enumerate() {
        let state = node.desired_state().await;
        assert_eq!(
            state.config.get("during_isolation").map(String::as_str),
            Some("committed"),
            "node {} missing the write after heal",
            i + 1
        );
    }
}

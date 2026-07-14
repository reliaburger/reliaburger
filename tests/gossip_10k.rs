//! 10,000-node deterministic gossip convergence acceptance test.
//!
//! This validates convergence correctness rather than benchmarking a shared
//! runner. It is ignored by default because the in-memory simulation is large.
//! Run it with `make bench-10k`.

use std::time::Instant;

#[path = "../benches/support/gossip.rs"]
mod gossip_support;

#[tokio::test]
async fn seeded_simulation_converges_without_a_sentinel_result() {
    let mut simulation = gossip_support::GossipSimulation::new(25).await;
    let rounds = simulation.converge(2_000).await.unwrap();
    assert!(rounds > 0);
}

#[tokio::test]
#[ignore = "10,000-node scale acceptance test; run with make bench-10k"]
async fn gossip_10k_nodes_converge() {
    let cluster_size = 10_000;

    eprintln!("setting up {cluster_size} nodes...");
    let setup_started = Instant::now();
    let mut simulation = gossip_support::GossipSimulation::new(cluster_size).await;
    eprintln!("setup took {:.1?}", setup_started.elapsed());

    let convergence_started = Instant::now();
    let rounds = simulation
        .converge(5_000)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let elapsed = convergence_started.elapsed();

    eprintln!("converged in {rounds} rounds ({elapsed:.1?})");
    eprintln!(
        "at the configured 50ms protocol interval, that represents {:.1}s of protocol time",
        rounds as f64 * 0.05
    );
}

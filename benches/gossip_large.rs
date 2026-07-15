//! Deterministic large-cluster gossip convergence benchmarks.
//!
//! These are deliberately separate from the fast benchmark target. Run with
//! `make bench-large`; the 10,000-member per-node scale acceptance lives
//! in `make bench-10k`.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

#[path = "support/gossip.rs"]
mod gossip_support;

fn bench_large_convergence(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let mut setup = c.benchmark_group("gossip_setup_large");
    setup.sample_size(10);
    setup.measurement_time(std::time::Duration::from_secs(5));
    for &size in &[500, 1_000] {
        setup.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| runtime.block_on(gossip_support::GossipSimulation::new(size)));
        });
    }
    setup.finish();

    let mut group = c.benchmark_group("gossip_convergence_large_end_to_end");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(10));

    for &size in &[500, 1_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                runtime.block_on(async {
                    let mut simulation = gossip_support::GossipSimulation::new(size).await;
                    simulation
                        .converge(5_000)
                        .await
                        .unwrap_or_else(|error| panic!("{error}"))
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_large_convergence);
criterion_main!(benches);

//! App placement benchmarks (#702).
//!
//! One leader scheduling pass (`plan_pass`) over 1,000 and 10,000 nodes,
//! in one process with no cluster:
//! - `plan_pass/place`: 50 new apps of 3 replicas each, so 150 placement
//!   decisions, into a cluster already a third full. Criterion reports
//!   decisions per second; its inverse is the cost of one decision.
//! - `plan_pass/converged`: the same cluster once every app is placed. The
//!   leader runs this pass every tick, so it is the steady-state cost.
//!
//! Run with `make bench` or `cargo bench --bench scheduler`.

use std::collections::{BTreeMap, HashMap, HashSet};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reliaburger::cluster::orchestrate::{PassMembers, plan_pass};
use reliaburger::config::app::AppSpec;
use reliaburger::council::types::DesiredState;
use reliaburger::meat::quota::QuotaLedger;
use reliaburger::meat::{AppId, ClusterStateCache, NodeId, Resources, SchedulerNodeState};

const APPS: usize = 50;
const REPLICAS: usize = 3;
const GIB: u64 = 1024 * 1024 * 1024;

/// `nodes` nodes of 8 cores and 16 GiB, each a third used, in three zones.
fn cluster(nodes: usize) -> ClusterStateCache {
    let mut cache = ClusterStateCache::new();
    for index in 0..nodes {
        let used = (index % 3) as u64;
        cache.set_node(SchedulerNodeState {
            node_id: NodeId::new(format!("node-{index:05}")),
            allocatable: Resources::new(8000, 16 * GIB, 0),
            allocated: Resources::new(2000 + used * 500, (4 + used * 2) * GIB, 0),
            labels: BTreeMap::from([("zone".to_string(), format!("z{}", index % 3))]),
            ready: true,
            // The benchmark's apps run images, which only a node with a
            // container runtime takes (#914).
            capabilities: reliaburger::meat::cluster_state::NodeCapabilities {
                containers: true,
                ..Default::default()
            },
            app_replicas: HashMap::new(),
            uptime_secs: 86400,
            cached_images: HashSet::new(),
        });
    }
    cache
}

/// `APPS` apps of `REPLICAS` replicas with mixed CPU and memory requests,
/// half of them preferring a zone.
fn desired() -> DesiredState {
    let mut desired = DesiredState::default();
    for index in 0..APPS {
        let preferred = if index % 2 == 0 {
            format!("\n[placement]\npreferred = [\"zone=z{}\"]", index % 3)
        } else {
            String::new()
        };
        let spec: AppSpec = toml::from_str(&format!(
            "image = \"app{index}:1\"\nreplicas = {REPLICAS}\n\
             cpu = \"{}m-2000m\"\nmemory = \"{}Mi-4Gi\"{preferred}",
            100 + (index % 7) * 50,
            128 + (index % 5) * 256,
        ))
        .expect("a valid app spec");
        desired
            .apps
            .insert(AppId::new(format!("app{index}"), "default"), spec);
    }
    desired
}

fn members(cache: &ClusterStateCache) -> PassMembers {
    PassMembers {
        alive: cache.node_ids().into_iter().collect(),
        ..Default::default()
    }
}

fn bench_plan_pass(c: &mut Criterion) {
    let mut group = c.benchmark_group("plan_pass");
    group.sample_size(10);
    for &nodes in &[1_000usize, 10_000] {
        let start = cluster(nodes);
        let members = members(&start);
        let new_apps = desired();

        group.throughput(Throughput::Elements((APPS * REPLICAS) as u64));
        group.bench_with_input(BenchmarkId::new("place", nodes), &nodes, |b, _| {
            b.iter_batched(
                || start.clone(),
                |mut cache| {
                    plan_pass(
                        &mut cache,
                        &new_apps,
                        &members,
                        &mut QuotaLedger::default(),
                        false,
                    )
                },
                BatchSize::LargeInput,
            );
        });

        // Commit that pass, so the next one finds every app placed.
        let mut placed = new_apps.clone();
        let plan = plan_pass(
            &mut start.clone(),
            &placed,
            &members,
            &mut QuotaLedger::default(),
            false,
        );
        assert_eq!(plan.decisions.len(), APPS, "every app is placed");
        for decision in plan.decisions {
            placed
                .scheduling
                .insert(decision.app_id, decision.placements);
        }
        group.throughput(Throughput::Elements(APPS as u64));
        group.bench_with_input(BenchmarkId::new("converged", nodes), &nodes, |b, _| {
            b.iter_batched(
                || start.clone(),
                |mut cache| {
                    let plan = plan_pass(
                        &mut cache,
                        &placed,
                        &members,
                        &mut QuotaLedger::default(),
                        false,
                    );
                    assert!(plan.decisions.is_empty());
                    plan
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_plan_pass);
criterion_main!(benches);

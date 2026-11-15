//! Policy evaluation benchmarks (#1311).
//!
//! What policy costs the apply path, per app, for 10, 100 and 1,000
//! policies in force. The leader runs the same two steps for every
//! `relish apply` and every GitOps sync:
//! - `policy/compile`: compile the policy set, once per apply (GitOps keeps
//!   it for a whole sync while the generation holds).
//! - `policy/judge_app`: judge one app: fill each matching default, then
//!   run every `match` and `deny`. Its time is the per-app cost.
//!
//! The set is the four platform policies of `examples/policies/platform.toml`,
//! which match every app, plus namespace-scoped copies of the same four
//! shapes for other teams. Those copies don't match the judged app, but each
//! one's `match` still runs, and a quarter of them carry a default, whose
//! `match` sees the app as JSON afresh. That's the shape a cluster with many
//! teams grows into, rather than a best case where nothing is evaluated.
//!
//! Run with `make bench` or `cargo bench --bench policy`.

use std::collections::BTreeMap;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reliaburger::config::{AppSpec, Config, PolicySpec};
use reliaburger::meat::AppId;
use reliaburger::policy::{Caller, PolicySet, Source, judge_app};
use reliaburger::sesame::types::ApiRole;

const PLATFORM: &str = include_str!("../examples/policies/platform.toml");

/// The four platform policies, then `count - 4` copies scoped to other
/// teams' namespaces, cycling through the same four shapes.
fn policies(count: usize) -> BTreeMap<String, PolicySpec> {
    let mut set = Config::parse(PLATFORM)
        .expect("the platform policies parse")
        .policy;
    for index in 0..count.saturating_sub(set.len()) {
        let team = format!("team-{index:04}");
        let scope = format!("match = \"app.namespace == '{team}'\"");
        let body = match index % 4 {
            0 => format!("{scope}\ndeny = \"!has(app.memory)\""),
            1 => format!("{scope}\ndeny = \"app.image.matches(':latest(@|$)')\""),
            2 => format!(
                "{scope}\ndeny = \"!['registry.example.com/{team}/'].exists(prefix, app.image.startsWith(prefix))\""
            ),
            _ => format!("{scope}\n[default]\nenv.TEAM = \"{team}\""),
        };
        let spec: PolicySpec = toml::from_str(&body).expect("a valid policy");
        set.insert(format!("{team}-rule"), spec);
    }
    assert_eq!(set.len(), count);
    set
}

/// One app that passes every policy: in `shop`, pinned, from a trusted
/// registry, with a limit, leaving `TEAM` for the default to fill.
fn app() -> (AppId, AppSpec) {
    let spec: AppSpec = toml::from_str(
        "namespace = \"shop\"\n\
         image = \"registry.example.com/shop/web:1.4\"\n\
         port = 8080\n\
         replicas = 3\n\
         memory = \"256Mi-1Gi\"\n\
         cpu = \"100m-500m\"\n\
         env = { LOG_LEVEL = \"info\", REGION = \"eu-west-1\" }\n",
    )
    .expect("a valid app spec");
    (AppId::new("web", "shop"), spec)
}

fn bench_policy(c: &mut Criterion) {
    let caller = Caller::token("operator", ApiRole::Admin);
    let (app_id, spec) = app();
    let mut group = c.benchmark_group("policy");
    for &count in &[10usize, 100, 1_000] {
        let specs = policies(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("compile", count), &specs, |b, specs| {
            b.iter(|| PolicySet::compile(specs).expect("the set compiles"));
        });

        let set = PolicySet::compile(&specs).expect("the set compiles");
        let judged = judge_app(&set, &app_id, &spec, &caller, Source::Apply);
        assert!(judged.verdict.is_allowed(), "{:?}", judged.verdict);
        assert_eq!(judged.filled.len(), 1, "only the platform default fills");
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("judge_app", count), &set, |b, set| {
            b.iter(|| judge_app(set, &app_id, &spec, &caller, Source::Apply));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_policy);
criterion_main!(benches);

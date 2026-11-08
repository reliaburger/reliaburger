//! `GET /v1/cluster/summary` and `GET /v1/cluster/capacity` through the
//! real router: what each section's source says when every node answers,
//! when a peer doesn't, and for a scoped token.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::NodeMembershipInfo;
use crate::bun::agent::BunAgent;
use crate::bun::summary::{ClusterCapacity, ClusterSummary, Source};
use crate::grill::mock::MockGrill;
use crate::grill::port::PortAllocator;
use crate::ketchup::log_store::LogStore;
use crate::ketchup::types::LogStream;
use crate::mayo::store::MayoStore;
use crate::mayo::types::{MetricKey, Sample};
use crate::meat::NodeId;
use crate::reporting::aggregator::AggregatedState;
use crate::reporting::types::{ResourceUsage, StateReport};

/// What a test router is built with.
#[derive(Default)]
struct Fixture {
    tokens: Vec<crate::sesame::types::ApiToken>,
    council: Option<Arc<crate::council::CouncilNode>>,
    membership: Vec<NodeMembershipInfo>,
    aggregated: Option<AggregatedState>,
    stores: bool,
}

/// A router over a running mock agent, named `node-1`.
fn router(fixture: Fixture, shutdown: &CancellationToken) -> (Router, Vec<tempfile::TempDir>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let mut agent = BunAgent::new(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        cmd_rx,
        shutdown.clone(),
    );
    tokio::spawn(async move { agent.run().await });
    let mut dirs = Vec::new();
    let (mayo, logs) = if fixture.stores {
        let metrics_dir = tempfile::tempdir().unwrap();
        let mut mayo = MayoStore::new(metrics_dir.path().to_path_buf());
        mayo.insert(
            &MetricKey::simple("node_cpu_usage_percent"),
            Sample::at(1_800_000_000, 42.0),
        );
        let logs_dir = tempfile::tempdir().unwrap();
        let mut logs = LogStore::new(logs_dir.path().to_path_buf());
        logs.append_at(1_800_000_005, "web", "default", LogStream::Stdout, "hello");
        dirs.extend([metrics_dir, logs_dir]);
        (
            Some(Arc::new(RwLock::new(mayo))),
            Some(Arc::new(RwLock::new(logs))),
        )
    } else {
        (None, None)
    };
    let tokens = (!fixture.tokens.is_empty()).then(|| Arc::new(RwLock::new(fixture.tokens)));
    let membership =
        (!fixture.membership.is_empty()).then(|| Arc::new(RwLock::new(fixture.membership)));
    let aggregated = fixture
        .aggregated
        .map(|state| tokio::sync::watch::channel(state).1);
    let app = super::router_with_upgrade(
        cmd_tx,
        mayo,
        logs,
        None,
        None,
        None,
        fixture.council,
        tokens,
        None,
        None,
        membership,
        None,
        None,
        9117,
        None,
        None,
        aggregated,
        "default".to_string(),
        Some("node-1".to_string()),
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        crate::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
        None,
    );
    let app = app.layer(axum::Extension(
        crate::mustard::stats::GossipStats::default(),
    ));
    (app, dirs)
}

async fn get(app: &Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = axum::http::Request::builder().uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

async fn summary(app: &Router, bearer: Option<&str>) -> ClusterSummary {
    let (status, body) = get(app, "/v1/cluster/summary", bearer).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn member(name: &str, address: &str) -> NodeMembershipInfo {
    NodeMembershipInfo {
        node_id: NodeId::new(name),
        address: address.parse().unwrap(),
        api_advertised: true,
        jobs: None,
    }
}

fn report(name: &str, gpu_used: u32, gpu_total: u32) -> StateReport {
    StateReport {
        has_buildah: false,
        node_id: NodeId::new(name),
        timestamp: std::time::SystemTime::now(),
        running_apps: Vec::new(),
        cached_specs: Vec::new(),
        rollouts: Vec::new(),
        resource_usage: ResourceUsage {
            cpu_used_millicores: 500,
            memory_used_mb: 1024,
            disk_used_mb: 0,
            gpu_used,
            gpu_total,
            allocated_ports: Vec::new(),
            cpu_total_millicores: 4000,
            memory_total_mb: 8192,
        },
        event_log: Vec::new(),
        job_reservations: Default::default(),
        capacity_refusals: Vec::new(),
    }
}

fn fresh(reports: Vec<StateReport>) -> AggregatedState {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    AggregatedState {
        leadership_epoch: None,
        receive_deadlines: reports
            .iter()
            .map(|report| (report.node_id.clone(), deadline))
            .collect(),
        reports: reports
            .into_iter()
            .map(|report| (report.node_id.clone(), report))
            .collect(),
        stale_nodes: Vec::new(),
        stale_since: HashMap::new(),
        capabilities: HashMap::new(),
        readiness: HashMap::new(),
    }
}

#[tokio::test]
async fn a_standalone_node_answers_with_every_section_marked() {
    let shutdown = CancellationToken::new();
    let (app, _dirs) = router(
        Fixture {
            stores: true,
            ..Fixture::default()
        },
        &shutdown,
    );
    let summary = summary(&app, None).await;
    assert_eq!(summary.answered_by, "node-1");
    assert!(!summary.scoped);
    assert_eq!(summary.apps.source, Source::Complete);
    assert_eq!(summary.council.source, Source::Complete);
    assert_eq!(summary.capacity.source, Source::Missing);
    assert_eq!(
        summary.capacity.reason.as_deref(),
        Some("a standalone node keeps no state reports")
    );
    let logs = summary.logs.value.unwrap();
    assert_eq!(logs.newest_unix_ms, Some(1_800_000_005_000));
    let usage = summary.usage.value.unwrap();
    assert_eq!(usage.newest_sample_unix_ms, Some(1_800_000_000_000));
    assert_eq!(usage.cpu_percent, Some(42.0));
    let gossip = summary.gossip.value.unwrap();
    assert_eq!(gossip.updates_received, 0);
    shutdown.cancel();
}

#[tokio::test]
async fn a_peer_that_does_not_answer_makes_the_summary_partial_and_names_it() {
    let shutdown = CancellationToken::new();
    // Nothing listens on port 9 of loopback, so the peer refuses at once.
    let (app, _dirs) = router(
        Fixture {
            membership: vec![
                member("node-1", "127.0.0.1:9117"),
                member("node-2", "127.0.0.1:9"),
            ],
            ..Fixture::default()
        },
        &shutdown,
    );
    let started = std::time::Instant::now();
    let summary = summary(&app, None).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(summary.apps.source, Source::Partial);
    let coverage = summary.apps.coverage.unwrap();
    assert_eq!((coverage.answered, coverage.asked), (1, 2));
    let reason = summary.apps.reason.unwrap();
    assert!(reason.starts_with("node-2: "), "{reason}");
    assert_eq!(summary.logs.source, Source::Partial);
    shutdown.cancel();
}

#[tokio::test]
async fn a_scoped_token_gets_its_own_view_and_no_cluster_wide_routes() {
    let shutdown = CancellationToken::new();
    let scoped = crate::sesame::token::create_token(
        "team-a",
        crate::sesame::types::ApiRole::ReadOnly,
        crate::sesame::types::TokenScope {
            apps: None,
            namespaces: Some(vec!["team-a".to_string()]),
        },
        None,
    )
    .unwrap();
    let (app, _dirs) = router(
        Fixture {
            tokens: vec![scoped.token],
            stores: true,
            ..Fixture::default()
        },
        &shutdown,
    );
    let summary = summary(&app, Some(&scoped.plaintext)).await;
    assert!(summary.scoped);
    for (name, section) in [
        ("capacity", summary.capacity.reason.as_deref()),
        ("usage", summary.usage.reason.as_deref()),
        ("logs", summary.logs.reason.as_deref()),
        ("alerts", summary.alerts.reason.as_deref()),
    ] {
        assert_eq!(
            section,
            Some(format!("{name} needs an unscoped token").as_str())
        );
    }
    assert_eq!(summary.apps.source, Source::Complete);
    for route in [
        "/v1/cluster/capacity",
        "/v1/cluster/summary/node",
        "/v1/cluster/summary/leader",
    ] {
        let (status, _) = get(&app, route, Some(&scoped.plaintext)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}");
    }
    shutdown.cancel();
}

#[tokio::test]
async fn the_capacity_route_counts_the_gpus_running_instances_hold() {
    let shutdown = CancellationToken::new();
    let council = super::tests::seeded_council("summary-gpu").await;
    let (app, _dirs) = router(
        Fixture {
            council: Some(council),
            membership: vec![member("node-1", "127.0.0.1:9117")],
            aggregated: Some(fresh(vec![report("node-1", 2, 4)])),
            ..Fixture::default()
        },
        &shutdown,
    );
    let (status, body) = get(&app, "/v1/cluster/capacity", None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let capacity: ClusterCapacity = serde_json::from_slice(&body).unwrap();
    assert_eq!(capacity.nodes.len(), 1);
    assert_eq!(
        (capacity.nodes[0].gpu_allocated, capacity.nodes[0].gpu_total),
        (2, 4)
    );
    assert_eq!(capacity.nodes[0].cpu_allocated_millicores, 500);

    let summary = summary(&app, None).await;
    assert_eq!(summary.capacity.source, Source::Complete);
    let totals = summary.capacity.value.unwrap();
    assert_eq!((totals.gpu_allocated, totals.gpu_total), (2, 4));
    let council = summary.council.value.unwrap();
    assert_eq!(council.leader.as_deref(), Some("node-1"));
    assert!(council.quorum_ok);
    shutdown.cancel();
}

#[tokio::test]
async fn a_live_node_without_a_fresh_report_makes_capacity_partial() {
    let shutdown = CancellationToken::new();
    let council = super::tests::seeded_council("summary-stale").await;
    let (app, _dirs) = router(
        Fixture {
            council: Some(council),
            membership: vec![
                member("node-1", "127.0.0.1:9117"),
                member("node-2", "127.0.0.1:9"),
            ],
            aggregated: Some(fresh(vec![report("node-1", 0, 0)])),
            ..Fixture::default()
        },
        &shutdown,
    );
    let summary = summary(&app, None).await;
    assert_eq!(summary.capacity.source, Source::Partial);
    assert_eq!(
        summary.capacity.reason.as_deref(),
        Some("node-2: no fresh state report")
    );
    shutdown.cancel();
}

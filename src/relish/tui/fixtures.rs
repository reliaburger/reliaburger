//! Deterministic data used by TUI unit and snapshot tests.

use std::collections::BTreeMap;

use crate::bun::agent::{
    ClusterInstanceStatus, CouncilStatus, InstanceStatus, JobStatus, NodeStatus,
};
use crate::bun::cluster_view::NodeTagged;
use crate::bun::diagnostics::DesiredAppEvidence;
use crate::bun::events::{ClusterEvent, EventKind, EventSeverity};
use crate::bun::summary::{
    AgentsSection, AlertsSection, AppBrief, AppState, AppsSection, CapacitySection, CertsSection,
    ClusterSummary, CouncilSection, Coverage, FaultsSection, GitOpsSection, GossipSection,
    InstancesSection, JobsSection, LogsSection, NodesSection, Section, UsageSection, VersionCount,
    VersionsSection,
};
use crate::mayo::rollup::{MetricsQueryResult, MetricsQueryRow};
use crate::wrapper::types::RouteInfo;

use super::app::{ClusterData, TuiApp};
use crate::bun::summary::{ClusterCapacity, NodeCapacity, NodeCouncilView, NodeSummary, NodeUsage};
use crate::relish::nodes_cmd::node_rows;

/// What a node says about itself in the fixtures: 1 GiB of 4 used, 30%
/// of its data disk.
fn node_share(node: &str, cpu_percent: f64) -> NodeSummary {
    NodeSummary {
        node: node.to_string(),
        version: "0.3.1".to_string(),
        council: NodeCouncilView::default(),
        apps: Vec::new(),
        newest_log_unix_ms: None,
        usage: NodeUsage {
            newest_sample_unix_ms: Some(FIXTURE_NOW * 1_000),
            cpu_percent: Some(cpu_percent),
            memory_used_bytes: Some(1_073_741_824.0),
            memory_total_bytes: Some(4_294_967_296.0),
            disk_used_bytes: None,
            disk_total_bytes: None,
        },
        data_disk_percent: Some(30.0),
        faults: Vec::new(),
        alerts: Vec::new(),
        certificate_not_after: None,
    }
}

pub const FIXTURE_NOW: u64 = 1_750_000_000;

/// Canned cluster shapes used across TUI tests.
#[derive(Debug, Clone, Copy)]
pub enum TestScenario {
    Empty,
    HealthyCluster,
    DegradedApp,
    ManyApps,
    /// Healthy, but node-3 didn't answer the summary in time.
    NodeMissing,
}

impl TestScenario {
    /// Build deterministic state for this scenario.
    pub fn data(self) -> ClusterData {
        let mut data = ClusterData::default();
        let count = match self {
            Self::Empty => 0,
            Self::ManyApps => 30,
            _ => 3,
        };
        for app_index in 0..count {
            let name = if app_index == 0 {
                "web".to_string()
            } else if app_index == 1 {
                "worker".to_string()
            } else {
                format!("app-{app_index}")
            };
            data.desired_apps.push(DesiredAppEvidence {
                app: name.clone(),
                namespace: "default".to_string(),
                desired_replicas: 2,
                scheduled_replicas: 2,
                placements: BTreeMap::new(),
                service_port: None,
                blocked: None,
                volume_home_away: None,
                unschedulable: None,
                volume_homes: Vec::new(),
                rollout_failed: None,
                owner: None,
            });
            for replica in 0..2 {
                let degraded = matches!(self, Self::DegradedApp) && app_index == 0 && replica == 1;
                data.instances.push(ClusterInstanceStatus {
                    node: format!("node-{}", replica + 1),
                    instance: InstanceStatus {
                        id: format!("{name}-{replica}"),
                        app_name: name.clone(),
                        namespace: "default".into(),
                        state: if degraded { "unhealthy" } else { "running" }.into(),
                        restart_count: u32::from(degraded) * 4,
                        host_port: Some(8000 + app_index as u16),
                        exit_code: None,
                        pid: Some(1000 + app_index as u32 * 2 + replica),
                        runtime_unknown: false,
                        status_age_ms: None,
                        age_seconds: None,
                        image: Some(format!("registry.example/{name}:v1")),
                        data_on_node: false,
                        sidecars: Vec::new(),
                        reason: None,
                    },
                });
            }
        }
        if !matches!(self, Self::Empty) {
            let members: Vec<NodeStatus> = (0..3)
                .map(|index| NodeStatus {
                    node_id: format!("node-{}", index + 1),
                    address: format!("10.0.0.{}:9118", index + 1),
                    api_address: None,
                    state: "alive".into(),
                    incarnation: 1,
                    is_council: true,
                    is_leader: index == 0,
                    labels: BTreeMap::from([("zone".into(), format!("z{}", index + 1))]),
                })
                .collect();
            // node-3 has no metrics sample yet, so its usage is unknown.
            let shares = [node_share("node-1", 12.0), node_share("node-2", 48.0)];
            let capacity = ClusterCapacity {
                nodes: (1..=3)
                    .map(|index| NodeCapacity {
                        node: format!("node-{index}"),
                        cpu_allocated_millicores: 1_000 * index,
                        cpu_total_millicores: 4_000,
                        memory_allocated_mb: 1_024,
                        memory_total_mb: 4_096,
                        ..Default::default()
                    })
                    .collect(),
                not_reporting: Vec::new(),
            };
            data.nodes = node_rows(members, &shares, &[], Some(&capacity));
            data.council = Some(CouncilStatus {
                leader: Some("node-1".into()),
                term: 4,
                app_count: count,
                ..Default::default()
            });
            data.routes.push(RouteInfo {
                host: "web.example.test".into(),
                path: "/".into(),
                app_name: "web".into(),
                namespace: "default".into(),
                healthy_backends: if matches!(self, Self::DegradedApp) {
                    1
                } else {
                    2
                },
                total_backends: 2,
                websocket: true,
            });
            data.jobs.push(NodeTagged {
                node: "node-1".into(),
                row: JobStatus {
                    name: "migrate".into(),
                    namespace: "default".into(),
                    instance_id: "migrate-0".into(),
                    image: "app:v1".into(),
                    state: "stopped".into(),
                    restart_count: 0,
                    age_seconds: 60,
                },
            });
            data.app_spec = fixture_web_spec();
            data.app_metrics = Some(MetricsQueryResult {
                data: vec![
                    MetricsQueryRow {
                        timestamp: FIXTURE_NOW - 2,
                        metric_name: "process_cpu_percent".into(),
                        labels: "{}".into(),
                        value: 12.0,
                    },
                    MetricsQueryRow {
                        timestamp: FIXTURE_NOW - 1,
                        metric_name: "process_cpu_percent".into(),
                        labels: "{}".into(),
                        value: 28.0,
                    },
                    MetricsQueryRow {
                        timestamp: FIXTURE_NOW,
                        metric_name: "process_memory_bytes".into(),
                        labels: "{}".into(),
                        value: 64.0,
                    },
                    MetricsQueryRow {
                        timestamp: FIXTURE_NOW,
                        metric_name: "http_requests_total".into(),
                        labels: r#"{"route":"/health"}"#.into(),
                        value: 1520.0,
                    },
                ],
                warnings: Vec::new(),
            });
            for index in 0..6 {
                data.events.push_back(ClusterEvent {
                    sequence: index + 1,
                    timestamp: FIXTURE_NOW - (6 - index) * 10,
                    kind: if index == 5 {
                        EventKind::Deploy
                    } else {
                        EventKind::Health
                    },
                    severity: if matches!(self, Self::DegradedApp) && index == 5 {
                        EventSeverity::Critical
                    } else {
                        EventSeverity::Info
                    },
                    app: Some("web".into()),
                    namespace: Some("default".into()),
                    node: None,
                    action: None,
                    principal: None,
                    source_address: None,
                    details: std::collections::BTreeMap::new(),
                    message: format!("fixture event {}", index + 1),
                });
            }
        }
        data.summary = Some(self.summary(count));
        data.last_updated = Some(FIXTURE_NOW);
        data
    }

    /// The summary of this scenario's cluster: `apps` apps of two replicas
    /// on three nodes.
    fn summary(self, apps: usize) -> ClusterSummary {
        let now_ms = FIXTURE_NOW * 1000;
        let apps = u32::try_from(apps).unwrap_or(u32::MAX);
        let (asked, answered) = match self {
            Self::NodeMissing => (3, 2),
            _ => (3, 3),
        };
        let coverage = Coverage { answered, asked };
        let reasons = match self {
            Self::NodeMissing => vec!["node-3: timed out".to_string()],
            _ => Vec::new(),
        };
        let degraded = u32::from(matches!(self, Self::DegradedApp));
        let worst = if degraded > 0 {
            vec![AppBrief {
                app: "web".into(),
                namespace: "default".into(),
                state: AppState::Degraded,
                desired: 2,
                running: 1,
            }]
        } else {
            Vec::new()
        };
        ClusterSummary {
            generated_at_unix_ms: now_ms,
            answered_by: "node-1".into(),
            version: "0.2.8".into(),
            scoped: false,
            nodes: Section::from_nodes(
                NodesSection {
                    alive: answered,
                    ..NodesSection::default()
                },
                coverage,
                &reasons,
            ),
            council: Section::complete(CouncilSection {
                leader: Some("node-1".into()),
                term: 4,
                voters: 3,
                voters_answering: 3,
                quorum_ok: true,
                lagging: 0,
                other_leaders: Vec::new(),
            }),
            gossip: Section::complete(GossipSection {
                members: 3,
                updates_received: 1_204,
                last_change_unix_ms: Some(now_ms - 600_000),
            }),
            versions: Section::from_nodes(
                VersionsSection {
                    versions: vec![VersionCount {
                        version: "0.2.8".into(),
                        nodes: answered,
                    }],
                    upgrade_in_progress: false,
                },
                coverage,
                &reasons,
            ),
            capacity: Section::complete(CapacitySection {
                nodes_reporting: 3,
                cpu_allocated_millicores: 2_000 * u64::from(apps),
                cpu_total_millicores: 24_000,
                memory_allocated_mb: 512 * u64::from(apps),
                memory_total_mb: 48_000,
                gpu_allocated: 0,
                gpu_total: 0,
            }),
            usage: Section::from_nodes(
                UsageSection {
                    cpu_percent: Some(21.0),
                    memory_percent: Some(40.0),
                    disk_percent: Some(30.0),
                    busiest: Vec::new(),
                    newest_sample_unix_ms: Some(now_ms - 5_000),
                },
                coverage,
                &reasons,
            ),
            apps: Section::from_nodes(
                AppsSection {
                    total: apps,
                    running: apps - degraded,
                    degraded,
                    worst,
                    ..AppsSection::default()
                },
                coverage,
                &reasons,
            ),
            instances: Section::from_nodes(
                InstancesSection {
                    running: apps * 2 - degraded,
                    desired: apps * 2,
                    not_running: degraded,
                },
                coverage,
                &reasons,
            ),
            jobs: Section::complete(JobsSection::default()),
            logs: Section::from_nodes(
                LogsSection {
                    newest_unix_ms: Some(now_ms - 2_000),
                    slowest: None,
                },
                coverage,
                &reasons,
            ),
            faults: Section::from_nodes(FaultsSection::default(), coverage, &reasons),
            alerts: Section::complete(AlertsSection::default()),
            certs: Section::from_nodes(
                CertsSection {
                    count: 3,
                    soonest: None,
                },
                coverage,
                &reasons,
            ),
            gitops: Section::complete(GitOpsSection {
                configured: false,
                commit: None,
                synced_at_unix_seconds: None,
                error: None,
            }),
            agents: Section::complete(AgentsSection::default()),
        }
    }
}

impl TuiApp {
    /// Create an app populated with deterministic fixture data.
    pub fn with_test_data(scenario: TestScenario) -> Self {
        let mut app = Self::new();
        app.data = scenario.data();
        app.now_epoch = FIXTURE_NOW;
        app
    }
}

/// `web` in `default` as the spec read answers it (#1322), its one env
/// value redacted.
fn fixture_web_spec() -> Option<crate::bun::spec_read::AppSpecView> {
    let spec: crate::config::AppSpec = toml::from_str(
        "image = 'app:v1'\nreplicas = 3\nport = 8080\nnamespace = 'default'\n\
         [env]\nDATABASE_URL = 'postgres://app:hunter2@db/app'\n",
    )
    .ok()?;
    let id = crate::meat::AppId::new("web", "default");
    let mut desired = crate::council::types::DesiredState::default();
    desired.apps.insert(id.clone(), spec);
    crate::bun::spec_read::app_spec_view(&desired, &id)
}

/// Render styles-agnostic terminal text for assertions and snapshots.
pub fn render_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| super::views::render(frame, app))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

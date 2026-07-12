//! Deterministic data used by TUI unit and snapshot tests.

use std::collections::BTreeMap;

use crate::bun::agent::{CouncilStatus, InstanceStatus, JobStatus, NodeStatus};
use crate::bun::events::{ClusterEvent, EventKind, EventSeverity};
use crate::mayo::rollup::{MetricsQueryResult, MetricsQueryRow};
use crate::wrapper::types::RouteInfo;

use super::app::{ClusterData, TuiApp};

pub const FIXTURE_NOW: u64 = 1_750_000_000;

/// Canned cluster shapes used across TUI tests.
#[derive(Debug, Clone, Copy)]
pub enum TestScenario {
    Empty,
    HealthyCluster,
    DegradedApp,
    ManyApps,
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
            for replica in 0..2 {
                let degraded = matches!(self, Self::DegradedApp) && app_index == 0 && replica == 1;
                data.instances.push(InstanceStatus {
                    id: format!("{name}-{replica}"),
                    app_name: name.clone(),
                    namespace: "default".into(),
                    state: if degraded { "unhealthy" } else { "running" }.into(),
                    restart_count: u32::from(degraded) * 4,
                    host_port: Some(8000 + app_index as u16),
                    exit_code: None,
                    pid: Some(1000 + app_index as u32 * 2 + replica),
                });
            }
        }
        if !matches!(self, Self::Empty) {
            for index in 0..3 {
                data.nodes.push(NodeStatus {
                    node_id: format!("node-{}", index + 1),
                    address: format!("10.0.0.{}:9118", index + 1),
                    state: "alive".into(),
                    incarnation: 1,
                    is_council: true,
                    is_leader: index == 0,
                    labels: BTreeMap::from([("zone".into(), format!("z{}", index + 1))]),
                });
            }
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
                healthy_backends: if matches!(self, Self::DegradedApp) {
                    1
                } else {
                    2
                },
                total_backends: 2,
                websocket: true,
            });
            data.jobs.push(JobStatus {
                name: "migrate".into(),
                namespace: "default".into(),
                instance_id: "migrate-0".into(),
                image: "proc-grill:image-ignored".into(),
                state: "stopped".into(),
                restart_count: 0,
                age_seconds: 60,
            });
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
                    message: format!("fixture event {}", index + 1),
                });
            }
        }
        data.last_updated = Some(FIXTURE_NOW);
        data
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

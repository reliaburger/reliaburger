//! Plain renderer-free state owned by the TUI event loop.

use std::collections::{HashMap, VecDeque};

use crate::bun::agent::{ClusterInstanceStatus, CouncilStatus, JobStatus};
use crate::bun::cluster_view::{ClusterDeployHistory, NodeTagged};
use crate::bun::events::ClusterEvent;
use crate::bun::summary::ClusterSummary;
use crate::mayo::rollup::MetricsQueryResult;
use crate::wrapper::types::RouteInfo;

use super::palette::PaletteState;

/// App detail tabs in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Instances,
    Logs,
    Metrics,
    Deploys,
    Config,
}

impl DetailTab {
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Instances,
        Self::Logs,
        Self::Metrics,
        Self::Deploys,
        Self::Config,
    ];

    pub(super) fn offset(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

/// Node detail tabs in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTab {
    /// What the node is: state, council seat, version and load.
    Overview,
    /// Every app instance running on it.
    Workloads,
    /// What would stop or lose redundancy if the node went away.
    Maintenance,
}

impl NodeTab {
    pub const ALL: [Self; 3] = [Self::Overview, Self::Workloads, Self::Maintenance];

    pub(super) fn offset(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

/// A screen in the navigation stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Apps,
    AppDetail {
        app: String,
        namespace: String,
        tab: DetailTab,
    },
    Nodes,
    NodeDetail {
        node: String,
        tab: NodeTab,
    },
    Jobs,
    /// One node's run of a job: several nodes can each run a job of the
    /// same name, so the node and instance pick the row.
    JobDetail {
        name: String,
        namespace: String,
        node: String,
        instance: String,
    },
    Events,
    Logs {
        app: Option<(String, String)>,
    },
    Routes,
    RouteDetail {
        host: String,
    },
    Search,
    Help,
}

/// Everything fetched from the agent.
#[derive(Debug, Clone, Default)]
pub struct ClusterData {
    /// The cluster at a glance (`GET /v1/cluster/summary`): what the
    /// dashboard and the header show. `None` until the first one arrives.
    pub summary: Option<ClusterSummary>,
    pub instances: Vec<ClusterInstanceStatus>,
    /// Every node as `relish nodes` lists it: gossip's view, its own
    /// usage and what the scheduler allocated there.
    pub nodes: Vec<crate::relish::nodes_cmd::NodeRow>,
    pub council: Option<CouncilStatus>,
    pub alerts: Vec<crate::mayo::alert::AlertStatus>,
    pub routes: Vec<RouteInfo>,
    /// Every node's jobs, each with its node.
    pub jobs: Vec<NodeTagged<JobStatus>>,
    /// Nodes whose jobs are missing from `jobs`.
    pub job_warnings: Vec<String>,
    pub events: VecDeque<ClusterEvent>,
    /// Nodes whose events are missing from the last refresh.
    pub event_warnings: Vec<String>,
    /// Deploy history keyed by [`deploy_history_key`] — two apps of the same
    /// name in different namespaces have separate histories (C3).
    pub deploy_history: HashMap<String, ClusterDeployHistory>,
    pub app_metrics: Option<MetricsQueryResult>,
    pub last_updated: Option<u64>,
}

/// Agent connection state.
#[derive(Debug, Clone)]
pub enum Connection {
    Connected,
    Disconnected {
        since_epoch: u64,
        last_error: String,
    },
}

/// A streamed log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub instance: String,
    pub line: String,
}

/// Search result destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: String,
    pub name: String,
    pub context: String,
    pub target: View,
}

/// Search input and current selection.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub input: String,
    pub results: Vec<SearchHit>,
    pub cursor: usize,
}

/// Status-message severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// One app instance on one node: what a kill from the Instances tab names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceTarget {
    pub app: String,
    pub namespace: String,
    pub instance: String,
    pub node: String,
}

/// Side effect requested by the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    RefreshAll,
    OpenLogStream {
        app: String,
        namespace: String,
    },
    CloseLogStream,
    FetchDeployHistory {
        app: String,
        namespace: String,
    },
    FetchAppMetrics {
        app: String,
        namespace: String,
    },
    /// Send Smoker's kill fault for one instance, as `relish fault kill`
    /// does. Only a `y` at the confirmation prompt queues it.
    KillInstance(InstanceTarget),
}

/// Cache key for [`ClusterData::deploy_history`].
///
/// Mirrors `AppId`'s `namespace/name` display form, so an app called `web`
/// in `team-a` never shows `team-b`'s deploys.
pub fn deploy_history_key(app: &str, namespace: &str) -> String {
    format!("{namespace}/{app}")
}

/// Complete state owned by the event loop.
pub struct TuiApp {
    pub view_stack: Vec<View>,
    pub data: ClusterData,
    pub connection: Connection,
    pub now_epoch: u64,
    pub apps_cursor: usize,
    pub nodes_cursor: usize,
    pub jobs_cursor: usize,
    pub routes_cursor: usize,
    /// The selected row in the app detail's Instances tab, or in a node
    /// detail's Workloads tab.
    pub instances_cursor: usize,
    /// The instance `k` asked to kill, while the `[y/N]` prompt is open.
    pub confirm_kill: Option<InstanceTarget>,
    pub events_scroll: usize,
    pub log_scroll: usize,
    pub(super) log_generation: u64,
    pub log_lines: VecDeque<LogLine>,
    pub log_follow: bool,
    pub search: SearchState,
    pub filter: Option<String>,
    pub filter_editing: bool,
    pub palette: Option<PaletteState>,
    pub status_message: Option<(String, Level)>,
    pub should_quit: bool,
    pub pending: Vec<Effect>,
    pub log_stream_down: Option<String>,
    pub event_stream_down: Option<String>,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiApp {
    /// Create an empty dashboard.
    pub fn new() -> Self {
        Self {
            view_stack: vec![View::Dashboard],
            data: ClusterData::default(),
            connection: Connection::Connected,
            now_epoch: 0,
            apps_cursor: 0,
            nodes_cursor: 0,
            jobs_cursor: 0,
            routes_cursor: 0,
            instances_cursor: 0,
            confirm_kill: None,
            events_scroll: 0,
            log_scroll: 0,
            log_generation: 0,
            log_lines: VecDeque::new(),
            log_follow: true,
            search: SearchState::default(),
            filter: None,
            filter_editing: false,
            palette: None,
            status_message: None,
            should_quit: false,
            pending: Vec::new(),
            log_stream_down: None,
            event_stream_down: None,
        }
    }

    /// Current view. The stack invariant keeps this available.
    pub fn view(&self) -> &View {
        self.view_stack.last().unwrap_or(&View::Dashboard)
    }

    pub fn all_apps(&self) -> Vec<(String, String)> {
        let mut apps: Vec<_> = self
            .data
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.instance.app_name.clone(),
                    instance.instance.namespace.clone(),
                )
            })
            .collect();
        apps.sort();
        apps.dedup();
        apps
    }

    /// One app's instances, in the order the cluster status listed them:
    /// the rows of the app detail's Instances tab.
    pub fn app_instances(&self, app: &str, namespace: &str) -> Vec<&ClusterInstanceStatus> {
        self.data
            .instances
            .iter()
            .filter(|row| row.instance.app_name == app && row.instance.namespace == namespace)
            .collect()
    }

    /// The instances running on one node, by namespace, app and instance:
    /// the rows of the node detail's Workloads tab.
    pub fn node_instances(&self, node: &str) -> Vec<&ClusterInstanceStatus> {
        let mut rows: Vec<_> = self
            .data
            .instances
            .iter()
            .filter(|row| row.node == node)
            .collect();
        rows.sort_by(|a, b| {
            (&a.instance.namespace, &a.instance.app_name, &a.instance.id).cmp(&(
                &b.instance.namespace,
                &b.instance.app_name,
                &b.instance.id,
            ))
        });
        rows
    }

    /// The Instances tab's selected row, if that tab is showing and has
    /// one. The cursor is clamped, since a refresh can shorten the list.
    pub fn selected_instance(&self) -> Option<InstanceTarget> {
        let View::AppDetail {
            app,
            namespace,
            tab: DetailTab::Instances,
        } = self.view()
        else {
            return None;
        };
        let rows = self.app_instances(app, namespace);
        let row = rows.get(self.instances_cursor.min(rows.len().saturating_sub(1)))?;
        Some(InstanceTarget {
            app: app.clone(),
            namespace: namespace.clone(),
            instance: row.instance.id.clone(),
            node: row.node.clone(),
        })
    }

    /// The buffered log lines the `/` search leaves, oldest first: all of
    /// them with no search, or those containing it, ignoring case.
    pub fn visible_log_lines(&self) -> Vec<&LogLine> {
        let needle = self.filter.as_deref().unwrap_or_default();
        self.log_lines
            .iter()
            .filter(|line| super::matching::contains(&line.line, needle))
            .collect()
    }

    pub fn filtered_apps(&self) -> Vec<(String, String)> {
        let apps = self.all_apps();
        match self.filter.as_deref() {
            Some(filter) if !filter.is_empty() => {
                let needle = filter.to_lowercase();
                apps.into_iter()
                    .filter(|(app, namespace)| {
                        app.to_lowercase().contains(&needle)
                            || namespace.to_lowercase().contains(&needle)
                    })
                    .collect()
            }
            _ => apps,
        }
    }
}

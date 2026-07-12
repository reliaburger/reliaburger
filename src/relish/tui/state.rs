//! Plain renderer-free state owned by the TUI event loop.

use std::collections::{HashMap, VecDeque};

use crate::bun::agent::{CouncilStatus, InstanceStatus, JobStatus, NodeStatus};
use crate::bun::events::ClusterEvent;
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
    },
    Jobs,
    JobDetail {
        name: String,
        namespace: String,
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
    pub instances: Vec<InstanceStatus>,
    pub nodes: Vec<NodeStatus>,
    pub council: Option<CouncilStatus>,
    pub alerts: Vec<serde_json::Value>,
    pub routes: Vec<RouteInfo>,
    pub jobs: Vec<JobStatus>,
    pub events: VecDeque<ClusterEvent>,
    pub deploy_history: HashMap<String, Vec<serde_json::Value>>,
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

/// Side effect requested by the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    RefreshAll,
    OpenLogStream { app: String, namespace: String },
    CloseLogStream,
    FetchDeployHistory { app: String },
    FetchAppMetrics { app: String, namespace: String },
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
    pub events_scroll: usize,
    pub log_scroll: usize,
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
            events_scroll: 0,
            log_scroll: 0,
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
            .map(|instance| (instance.app_name.clone(), instance.namespace.clone()))
            .collect();
        apps.sort();
        apps.dedup();
        apps
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

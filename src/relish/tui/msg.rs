//! Messages exchanged by TUI input, polling, and streaming tasks.

use crossterm::event::KeyEvent;

use crate::bun::agent::{ClusterInstanceStatus, CouncilStatus, JobStatus, NodeStatus};
use crate::bun::events::ClusterEvent;
use crate::mayo::rollup::MetricsQueryResult;
use crate::wrapper::types::RouteInfo;

use super::app::LogLine;
use super::data::ProviderError;

/// An input or background update delivered to the reducer.
#[derive(Debug)]
pub enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// Terminal dimensions changed.
    Resize(u16, u16),
    /// Advance the render clock.
    Tick,
    /// A completed HTTP fetch.
    Data(DataUpdate),
    /// An item from a live WebSocket.
    Stream(StreamItem),
    /// A log subscription update tagged so a cancelled stream cannot affect its successor.
    LogStream { generation: u64, item: StreamItem },
}

/// Results from HTTP data providers.
#[derive(Debug)]
pub enum DataUpdate {
    Status(Result<Vec<ClusterInstanceStatus>, ProviderError>),
    Nodes(Result<Vec<NodeStatus>, ProviderError>),
    Council(Result<CouncilStatus, ProviderError>),
    Alerts(Result<Vec<crate::mayo::alert::AlertStatus>, ProviderError>),
    Routes(Result<Vec<RouteInfo>, ProviderError>),
    Jobs(Result<Vec<JobStatus>, ProviderError>),
    EventsSeed(Result<Vec<ClusterEvent>, ProviderError>),
    DeployHistory {
        app: String,
        namespace: String,
        result: Result<Vec<serde_json::Value>, ProviderError>,
    },
    AppMetrics(Result<MetricsQueryResult, ProviderError>),
}

/// Live stream identifiers used in connection banners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Logs,
    Events,
}

/// An item produced by a live stream task.
#[derive(Debug)]
pub enum StreamItem {
    LogLine(LogLine),
    Event(ClusterEvent),
    StreamDown { what: StreamKind, error: String },
    StreamUp { what: StreamKind },
}

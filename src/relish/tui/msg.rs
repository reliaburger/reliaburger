//! Messages exchanged by TUI input, polling, and streaming tasks.

use crossterm::event::KeyEvent;

use crate::bun::agent::{ClusterInstanceStatus, CouncilStatus};
use crate::bun::cluster_view::{ClusterDeployHistory, ClusterEvents, ClusterJobs};
use crate::bun::events::ClusterEvent;
use crate::bun::summary::ClusterSummary;
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
    /// Boxed: a summary is over a kilobyte, and every other message would
    /// pay for that space.
    Summary(Result<Box<ClusterSummary>, ProviderError>),
    Status(Result<Vec<ClusterInstanceStatus>, ProviderError>),
    Nodes(Result<Vec<crate::relish::nodes_cmd::NodeRow>, ProviderError>),
    Council(Result<CouncilStatus, ProviderError>),
    Alerts(Result<Vec<crate::mayo::alert::AlertStatus>, ProviderError>),
    Routes(Result<Vec<RouteInfo>, ProviderError>),
    Jobs(Result<ClusterJobs, ProviderError>),
    EventsSeed(Result<ClusterEvents, ProviderError>),
    DeployHistory {
        app: String,
        namespace: String,
        result: Result<ClusterDeployHistory, ProviderError>,
    },
    AppMetrics(Result<MetricsQueryResult, ProviderError>),
    /// The Config tab's spec read. Boxed, like the summary: a spec is over
    /// two kilobytes.
    AppSpec {
        app: String,
        namespace: String,
        result: Result<Box<crate::bun::spec_read::AppSpecView>, ProviderError>,
    },
    DesiredApps(Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, ProviderError>),
    /// The answer to a scale the person confirmed.
    Scaled {
        app: String,
        to: u32,
        result: Result<(), ProviderError>,
    },
    /// The answer to a kill from the Instances tab.
    InstanceKilled {
        target: super::state::InstanceTarget,
        result: Result<crate::smoker::types::FaultSummary, ProviderError>,
    },
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
    /// A node's part of a cluster-wide log follow stopped.
    LogWarning(String),
    Event(ClusterEvent),
    StreamDown {
        what: StreamKind,
        error: String,
    },
    StreamUp {
        what: StreamKind,
    },
}

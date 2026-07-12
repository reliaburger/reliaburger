//! HTTP and deterministic test data providers.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bun::agent::{CouncilStatus, InstanceStatus, JobStatus, NodeStatus};
use crate::bun::events::ClusterEvent;
use crate::mayo::rollup::MetricsQueryResult;
use crate::relish::client::BunClient;
use crate::wrapper::types::RouteInfo;

use super::fixtures::TestScenario;
use super::msg::StreamItem;

/// Failures surfaced by a data provider.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("agent unreachable: {0}")]
    Unreachable(String),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("websocket: {0}")]
    WebSocket(String),
}

impl From<crate::relish::RelishError> for ProviderError {
    fn from(error: crate::relish::RelishError) -> Self {
        match error {
            crate::relish::RelishError::ApiError { status, body } => Self::Api { status, body },
            crate::relish::RelishError::WebSocket(message) => Self::WebSocket(message),
            other => Self::Unreachable(other.to_string()),
        }
    }
}

/// All data and stream operations used by the event loop.
pub trait DataProvider: Send + Sync + 'static {
    fn status(&self) -> impl Future<Output = Result<Vec<InstanceStatus>, ProviderError>> + Send;
    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeStatus>, ProviderError>> + Send;
    fn council(&self) -> impl Future<Output = Result<CouncilStatus, ProviderError>> + Send;
    fn alerts(&self) -> impl Future<Output = Result<Vec<serde_json::Value>, ProviderError>> + Send;
    fn routes(&self) -> impl Future<Output = Result<Vec<RouteInfo>, ProviderError>> + Send;
    fn jobs(&self) -> impl Future<Output = Result<Vec<JobStatus>, ProviderError>> + Send;
    fn recent_events(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<ClusterEvent>, ProviderError>> + Send;
    fn deploy_history(
        &self,
        app: &str,
    ) -> impl Future<Output = Result<Vec<serde_json::Value>, ProviderError>> + Send;
    fn app_metrics(
        &self,
        app: &str,
        namespace: &str,
    ) -> impl Future<Output = Result<MetricsQueryResult, ProviderError>> + Send;
    fn follow_logs(
        &self,
        app: String,
        namespace: String,
        tail: usize,
        tx: mpsc::Sender<StreamItem>,
        cancel: CancellationToken,
    ) -> impl Future<Output = ()> + Send;
    fn follow_events(
        &self,
        tx: mpsc::Sender<StreamItem>,
        cancel: CancellationToken,
    ) -> impl Future<Output = ()> + Send;
}

/// Provider backed by the real Bun HTTP API.
pub struct HttpDataProvider {
    pub client: BunClient,
}

impl DataProvider for HttpDataProvider {
    async fn status(&self) -> Result<Vec<InstanceStatus>, ProviderError> {
        self.client.status().await.map_err(Into::into)
    }
    async fn nodes(&self) -> Result<Vec<NodeStatus>, ProviderError> {
        self.client.nodes().await.map_err(Into::into)
    }
    async fn council(&self) -> Result<CouncilStatus, ProviderError> {
        self.client.council().await.map_err(Into::into)
    }
    async fn alerts(&self) -> Result<Vec<serde_json::Value>, ProviderError> {
        self.client.alerts().await.map_err(Into::into)
    }
    async fn routes(&self) -> Result<Vec<RouteInfo>, ProviderError> {
        self.client.routes().await.map_err(Into::into)
    }
    async fn jobs(&self) -> Result<Vec<JobStatus>, ProviderError> {
        self.client.jobs().await.map_err(Into::into)
    }
    async fn recent_events(&self, limit: usize) -> Result<Vec<ClusterEvent>, ProviderError> {
        self.client.events(limit).await.map_err(Into::into)
    }
    async fn deploy_history(&self, app: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
        self.client.deploy_history(app).await.map_err(Into::into)
    }
    async fn app_metrics(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<MetricsQueryResult, ProviderError> {
        self.client
            .app_metrics(app, namespace)
            .await
            .map_err(Into::into)
    }

    async fn follow_logs(
        &self,
        app: String,
        namespace: String,
        tail: usize,
        tx: mpsc::Sender<StreamItem>,
        cancel: CancellationToken,
    ) {
        super::stream::reconnect_logs(&self.client, app, namespace, tail, tx, cancel).await;
    }

    async fn follow_events(&self, tx: mpsc::Sender<StreamItem>, cancel: CancellationToken) {
        super::stream::reconnect_events(&self.client, tx, cancel).await;
    }
}

/// Deterministic provider used by reducer and renderer tests.
pub struct MockDataProvider {
    pub scenario: TestScenario,
}

impl DataProvider for MockDataProvider {
    async fn status(&self) -> Result<Vec<InstanceStatus>, ProviderError> {
        Ok(self.scenario.data().instances)
    }
    async fn nodes(&self) -> Result<Vec<NodeStatus>, ProviderError> {
        Ok(self.scenario.data().nodes)
    }
    async fn council(&self) -> Result<CouncilStatus, ProviderError> {
        Ok(self.scenario.data().council.unwrap_or_default())
    }
    async fn alerts(&self) -> Result<Vec<serde_json::Value>, ProviderError> {
        Ok(self.scenario.data().alerts)
    }
    async fn routes(&self) -> Result<Vec<RouteInfo>, ProviderError> {
        Ok(self.scenario.data().routes)
    }
    async fn jobs(&self) -> Result<Vec<JobStatus>, ProviderError> {
        Ok(self.scenario.data().jobs)
    }
    async fn recent_events(&self, limit: usize) -> Result<Vec<ClusterEvent>, ProviderError> {
        Ok(self
            .scenario
            .data()
            .events
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect())
    }
    async fn deploy_history(&self, app: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
        Ok(self
            .scenario
            .data()
            .deploy_history
            .get(app)
            .cloned()
            .unwrap_or_default())
    }
    async fn app_metrics(
        &self,
        _app: &str,
        _namespace: &str,
    ) -> Result<MetricsQueryResult, ProviderError> {
        Ok(self
            .scenario
            .data()
            .app_metrics
            .unwrap_or(MetricsQueryResult {
                data: Vec::new(),
                warnings: Vec::new(),
            }))
    }
    async fn follow_logs(
        &self,
        _app: String,
        _namespace: String,
        _tail: usize,
        _tx: mpsc::Sender<StreamItem>,
        cancel: CancellationToken,
    ) {
        cancel.cancelled().await;
    }
    async fn follow_events(&self, _tx: mpsc::Sender<StreamItem>, cancel: CancellationToken) {
        cancel.cancelled().await;
    }
}

/// Spawn a complete fetch and send one message per result.
pub fn spawn_refresh<P: DataProvider>(provider: Arc<P>, tx: mpsc::Sender<super::msg::Msg>) {
    tokio::spawn(async move {
        let (status, nodes, council, alerts, routes, jobs, events) = tokio::join!(
            provider.status(),
            provider.nodes(),
            provider.council(),
            provider.alerts(),
            provider.routes(),
            provider.jobs(),
            provider.recent_events(100)
        );
        for update in [
            super::msg::DataUpdate::Status(status),
            super::msg::DataUpdate::Nodes(nodes),
            super::msg::DataUpdate::Council(council),
            super::msg::DataUpdate::Alerts(alerts),
            super::msg::DataUpdate::Routes(routes),
            super::msg::DataUpdate::Jobs(jobs),
            super::msg::DataUpdate::EventsSeed(events),
        ] {
            if tx.send(super::msg::Msg::Data(update)).await.is_err() {
                return;
            }
        }
    });
}

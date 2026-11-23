//! HTTP and deterministic test data providers.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bun::agent::{ClusterInstanceStatus, CouncilStatus};
use crate::bun::cluster_view::{ClusterDeployHistory, ClusterEvents, ClusterJobs};
use crate::bun::diagnostics::DesiredAppEvidence;
use crate::bun::summary::ClusterSummary;
use crate::mayo::rollup::MetricsQueryResult;
use crate::relish::client::BunClient;
use crate::relish::nodes_cmd::NodeRow;
use crate::wrapper::types::RouteInfo;

use crate::smoker::types::FaultSummary;

use super::fixtures::TestScenario;
use super::msg::StreamItem;
use super::state::InstanceTarget;

/// The reason a kill from the terminal UI carries into the fault record
/// and the audit log.
const KILL_REASON: &str = "relish tui";

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
    fn summary(&self) -> impl Future<Output = Result<ClusterSummary, ProviderError>> + Send;
    fn status(
        &self,
    ) -> impl Future<Output = Result<Vec<ClusterInstanceStatus>, ProviderError>> + Send;
    /// Every node's `relish nodes` row.
    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeRow>, ProviderError>> + Send;
    fn council(&self) -> impl Future<Output = Result<CouncilStatus, ProviderError>> + Send;
    fn alerts(
        &self,
    ) -> impl Future<Output = Result<Vec<crate::mayo::alert::AlertStatus>, ProviderError>> + Send;
    fn routes(&self) -> impl Future<Output = Result<Vec<RouteInfo>, ProviderError>> + Send;
    fn jobs(&self) -> impl Future<Output = Result<ClusterJobs, ProviderError>> + Send;
    fn recent_events(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<ClusterEvents, ProviderError>> + Send;
    fn deploy_history(
        &self,
        app: &str,
        namespace: &str,
    ) -> impl Future<Output = Result<ClusterDeployHistory, ProviderError>> + Send;
    fn app_metrics(
        &self,
        app: &str,
        namespace: &str,
    ) -> impl Future<Output = Result<MetricsQueryResult, ProviderError>> + Send;
    fn desired_apps(
        &self,
    ) -> impl Future<Output = Result<Vec<DesiredAppEvidence>, ProviderError>> + Send;
    /// Set an app's replica count to `to`, if its spec still asks for
    /// `from` (#2176).
    fn scale(
        &self,
        app: &str,
        namespace: &str,
        from: u32,
        to: u32,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;
    /// Kill one instance with Smoker's kill fault, as `relish fault kill`
    /// does with `--instance` and `--node`.
    fn kill_instance(
        &self,
        target: &InstanceTarget,
    ) -> impl Future<Output = Result<FaultSummary, ProviderError>> + Send;
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
    async fn summary(&self) -> Result<ClusterSummary, ProviderError> {
        self.client.cluster_summary().await.map_err(Into::into)
    }
    async fn status(&self) -> Result<Vec<ClusterInstanceStatus>, ProviderError> {
        self.client.cluster_status().await.map_err(Into::into)
    }
    async fn nodes(&self) -> Result<Vec<NodeRow>, ProviderError> {
        // A missing allocation answer leaves `-` in the ALLOCATED column;
        // the TUI has no stderr to warn on.
        let (rows, _allocations_error) = crate::relish::nodes_cmd::fetch_rows(&self.client).await?;
        Ok(rows)
    }
    async fn council(&self) -> Result<CouncilStatus, ProviderError> {
        self.client.council().await.map_err(Into::into)
    }
    async fn alerts(&self) -> Result<Vec<crate::mayo::alert::AlertStatus>, ProviderError> {
        self.client.alerts().await.map_err(Into::into)
    }
    async fn routes(&self) -> Result<Vec<RouteInfo>, ProviderError> {
        self.client.routes().await.map_err(Into::into)
    }
    async fn jobs(&self) -> Result<ClusterJobs, ProviderError> {
        self.client.jobs().await.map_err(Into::into)
    }
    async fn recent_events(&self, limit: usize) -> Result<ClusterEvents, ProviderError> {
        self.client.events(limit).await.map_err(Into::into)
    }
    async fn deploy_history(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<ClusterDeployHistory, ProviderError> {
        self.client
            .deploy_history(app, namespace)
            .await
            .map_err(Into::into)
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
    async fn desired_apps(&self) -> Result<Vec<DesiredAppEvidence>, ProviderError> {
        self.client.desired_apps().await.map_err(Into::into)
    }
    async fn scale(
        &self,
        app: &str,
        namespace: &str,
        from: u32,
        to: u32,
    ) -> Result<(), ProviderError> {
        self.client
            .scale(app, namespace, to, Some(from), false)
            .await
            .map_err(Into::into)
    }

    async fn kill_instance(&self, target: &InstanceTarget) -> Result<FaultSummary, ProviderError> {
        let request = crate::relish::fault::instance_kill_request(
            &target.app,
            &target.namespace,
            &target.instance,
            &target.node,
            KILL_REASON,
        );
        self.client.inject_fault(&request).await.map_err(Into::into)
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
    async fn summary(&self) -> Result<ClusterSummary, ProviderError> {
        self.scenario.data().summary.ok_or(ProviderError::Api {
            status: 404,
            body: "no summary in this scenario".to_string(),
        })
    }
    async fn status(&self) -> Result<Vec<ClusterInstanceStatus>, ProviderError> {
        Ok(self.scenario.data().instances)
    }
    async fn nodes(&self) -> Result<Vec<NodeRow>, ProviderError> {
        Ok(self.scenario.data().nodes)
    }
    async fn council(&self) -> Result<CouncilStatus, ProviderError> {
        Ok(self.scenario.data().council.unwrap_or_default())
    }
    async fn alerts(&self) -> Result<Vec<crate::mayo::alert::AlertStatus>, ProviderError> {
        Ok(self.scenario.data().alerts)
    }
    async fn routes(&self) -> Result<Vec<RouteInfo>, ProviderError> {
        Ok(self.scenario.data().routes)
    }
    async fn jobs(&self) -> Result<ClusterJobs, ProviderError> {
        let data = self.scenario.data();
        Ok(ClusterJobs {
            jobs: data.jobs,
            warnings: data.job_warnings,
        })
    }
    async fn recent_events(&self, limit: usize) -> Result<ClusterEvents, ProviderError> {
        let data = self.scenario.data();
        let skip = data.events.len().saturating_sub(limit);
        Ok(ClusterEvents {
            events: data.events.into_iter().skip(skip).collect(),
            warnings: data.event_warnings,
        })
    }
    async fn deploy_history(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<ClusterDeployHistory, ProviderError> {
        Ok(self
            .scenario
            .data()
            .deploy_history
            .remove(&super::state::deploy_history_key(app, namespace))
            .unwrap_or_else(|| ClusterDeployHistory {
                app: app.to_string(),
                namespace: namespace.to_string(),
                history: Vec::new(),
                warnings: Vec::new(),
            }))
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
    async fn desired_apps(&self) -> Result<Vec<DesiredAppEvidence>, ProviderError> {
        Ok(self.scenario.data().desired_apps)
    }
    async fn scale(
        &self,
        _app: &str,
        _namespace: &str,
        _from: u32,
        _to: u32,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn kill_instance(&self, target: &InstanceTarget) -> Result<FaultSummary, ProviderError> {
        Ok(FaultSummary {
            id: 1,
            fault_type: "kill".into(),
            target_service: target.app.clone(),
            namespace: Some(target.namespace.clone()),
            target_instance: Some(target.instance.clone()),
            task_index: None,
            target_node: Some(target.node.clone()),
            remaining_secs: 0,
            injected_by: "test".into(),
            node: Some(target.node.clone()),
            routed: Vec::new(),
        })
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
        let (summary, status, nodes, council, alerts, routes, jobs, events, desired) = tokio::join!(
            provider.summary(),
            provider.status(),
            provider.nodes(),
            provider.council(),
            provider.alerts(),
            provider.routes(),
            provider.jobs(),
            provider.recent_events(100),
            provider.desired_apps()
        );
        for update in [
            super::msg::DataUpdate::Summary(summary.map(Box::new)),
            super::msg::DataUpdate::Status(status),
            super::msg::DataUpdate::Nodes(nodes),
            super::msg::DataUpdate::Council(council),
            super::msg::DataUpdate::Alerts(alerts),
            super::msg::DataUpdate::Routes(routes),
            super::msg::DataUpdate::Jobs(jobs),
            super::msg::DataUpdate::EventsSeed(events),
            super::msg::DataUpdate::DesiredApps(desired),
        ] {
            if tx.send(super::msg::Msg::Data(update)).await.is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::Query, routing::get};

    #[tokio::test]
    async fn status_requests_cluster_replicas_and_keeps_node_and_namespace() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/status", get(|Query(query): Query<std::collections::HashMap<String, String>>| async move {
            if query.get("cluster").map(String::as_str) == Some("true") {
                Json(serde_json::json!([{"node":"node-b", "id":"web-0", "app_name":"web", "namespace":"production", "state":"running", "restart_count":0, "host_port":8080, "exit_code":null, "pid":123}]))
            } else { Json(serde_json::json!([])) }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = HttpDataProvider {
            client: BunClient::new(&format!("http://{address}")),
        };
        let result = provider.status().await;
        server.abort();
        let _ = server.await;
        let statuses = result.unwrap();
        assert_eq!(statuses.len(), 1, "remote-only replica vanished");
        let json = serde_json::to_value(statuses).unwrap();
        assert_eq!(json[0]["node"], "node-b");
        assert_eq!(json[0]["namespace"], "production");
    }

    /// Serve `/v1/fault`, keeping each request, and answer with `status`
    /// and `body`, or with a fault summary on 200.
    async fn serve_fault(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<crate::smoker::types::FaultRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::routing::post;

        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = Router::new().route(
            "/v1/fault",
            post({
                let seen = Arc::clone(&seen);
                move |Json(request): Json<crate::smoker::types::FaultRequest>| async move {
                    seen.lock().await.push(request);
                    let code = axum::http::StatusCode::from_u16(status).unwrap();
                    if status != 200 {
                        return axum::response::IntoResponse::into_response((code, body));
                    }
                    axum::response::IntoResponse::into_response(Json(serde_json::json!({
                        "id": 7, "fault_type": "kill", "target_service": "web",
                        "namespace": "default", "target_instance": "web-1",
                        "target_node": "node-2", "remaining_secs": 0,
                        "injected_by": "admin"
                    })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), seen, server)
    }

    fn web_1_on_node_2() -> crate::relish::tui::state::InstanceTarget {
        crate::relish::tui::state::InstanceTarget {
            app: "web".into(),
            namespace: "default".into(),
            instance: "web-1".into(),
            node: "node-2".into(),
        }
    }

    // The TUI's kill is `relish fault kill web --namespace default
    // --instance web-1 --node node-2 --acknowledge --reason "relish tui"`,
    // sent to the same API, so it's audited and rail-checked the same way
    // (#2175).
    #[tokio::test]
    async fn kill_instance_sends_the_smoker_kill_fault_for_that_instance_and_node() {
        let (url, seen, server) = serve_fault(200, "").await;
        let provider = HttpDataProvider {
            client: BunClient::new(&url),
        };
        let summary = provider.kill_instance(&web_1_on_node_2()).await.unwrap();
        server.abort();
        assert_eq!(summary.id, 7);
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 1);
        let request = &seen[0];
        assert!(matches!(
            request.fault_type,
            crate::smoker::types::FaultType::Kill { count: 1 }
        ));
        assert_eq!(request.target_service, "web");
        assert_eq!(request.namespace.as_deref(), Some("default"));
        assert_eq!(request.target_instance.as_deref(), Some("web-1"));
        assert_eq!(request.target_node.as_deref(), Some("node-2"));
        assert_eq!(request.reason.as_deref(), Some("relish tui"));
        assert!(request.acknowledged);
        assert!(!request.override_safety, "the TUI never overrides a rail");
        assert_eq!(request.duration, std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn a_refused_kill_comes_back_with_the_apis_words() {
        let (url, _seen, server) =
            serve_fault(403, "safety rail: would kill the last replica").await;
        let provider = HttpDataProvider {
            client: BunClient::new(&url),
        };
        let result = provider.kill_instance(&web_1_on_node_2()).await;
        server.abort();
        assert!(
            matches!(&result, Err(ProviderError::Api { status: 403, body }) if body.contains("last replica")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn partial_cluster_status_remains_an_explicit_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v1/status",
            get(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "node-c did not answer; cluster status is incomplete",
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = HttpDataProvider {
            client: BunClient::new(&format!("http://{address}")),
        };
        let result = provider.status().await;
        server.abort();
        let _ = server.await;
        assert!(
            matches!(result, Err(ProviderError::Api { status: 503, body }) if body.contains("node-c"))
        );
    }
}

//! Bounded, authenticated producer retirement against the current leader.

use crate::council::types::CouncilNodeInfo;
use crate::grill::RuntimeExecution;
use crate::mustard::directory::NodeDirectory;
use crate::onion::producer::ProducerReleaseConfirmation;
use std::io;
use tokio::sync::watch;

/// Transport and live leader information for producer release requests.
#[derive(Clone)]
pub struct ProducerReleaseClient {
    http: super::ClusterHttp,
    metrics: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    directory: watch::Receiver<NodeDirectory>,
    raft_to_api_offset: i32,
    #[cfg(test)]
    pub(crate) allow_plaintext: bool,
}

impl ProducerReleaseClient {
    /// Use the enrolled node's mTLS client and service authority; plaintext cannot release ownership.
    pub fn new(
        http: super::ClusterHttp,
        metrics: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
        directory: watch::Receiver<NodeDirectory>,
        raft_to_api_offset: i32,
    ) -> Self {
        Self {
            http,
            metrics,
            directory,
            raft_to_api_offset,
            #[cfg(test)]
            allow_plaintext: false,
        }
    }

    pub(crate) async fn confirm(
        &self,
        node_id: &str,
        execution: &RuntimeExecution,
    ) -> io::Result<ProducerReleaseConfirmation> {
        let secure = self.http.scheme() == "https";
        #[cfg(test)]
        let secure = secure || self.allow_plaintext;
        if !secure {
            return Err(io::Error::other(
                "producer release requires authenticated HTTPS",
            ));
        }
        let address = super::directory::resolve_leader(
            &self.metrics.borrow(),
            &self.directory.borrow(),
            self.raft_to_api_offset,
            0,
        )
        .and_then(|leader| leader.api_address)
        .ok_or_else(|| io::Error::other("producer release has no known leader"))?;
        let url = self.http.url(&address.to_string(), "/v1/discovery/retire");
        let request = crate::onion::producer::ProducerRetirementRequest {
            compatibility: crate::compatibility::CURRENT,
            execution: execution.clone(),
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut request = self.http.client().post(&url).json(&request);
            if let Some(token) = self.http.bearer() {
                request = request.bearer_auth(token);
            }
            let mut response = request.send().await.map_err(io::Error::other)?;
            if response.url().as_str() != url || response.status() != reqwest::StatusCode::OK {
                return Err(io::Error::other(format!(
                    "producer release is unconfirmed ({})",
                    response.status()
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
                if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
                    return Err(io::Error::other("producer confirmation exceeds size limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let confirmation: ProducerReleaseConfirmation =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if confirmation.node_id != node_id || confirmation.execution != *execution {
                return Err(io::Error::other(
                    "producer confirmation belongs to another identity or execution",
                ));
            }
            Ok(confirmation)
        })
        .await
        .map_err(|_| io::Error::other("producer release timed out; original ownership retained"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, response::IntoResponse, routing::post};

    pub(crate) async fn fixture(
        status: axum::http::StatusCode,
        body: String,
    ) -> (ProducerReleaseClient, tokio::task::JoinHandle<()>) {
        delayed_fixture(status, body, std::time::Duration::ZERO).await
    }

    pub(crate) async fn delayed_fixture(
        status: axum::http::StatusCode,
        body: String,
        delay: std::time::Duration,
    ) -> (ProducerReleaseClient, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/discovery/retire",
            post(move || {
                let body = body.clone();
                async move {
                    tokio::time::sleep(delay).await;
                    (status, body).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (_, metrics) = watch::channel(openraft::RaftMetrics::new_initial(1));
        let (_, directory) = watch::channel(NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: crate::meat::NodeId::new("leader"),
                term: 1,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        let mut client = ProducerReleaseClient::new(
            super::super::ClusterHttp::plaintext(),
            metrics,
            directory,
            0,
        );
        client.allow_plaintext = true;
        (client, task)
    }

    fn execution() -> RuntimeExecution {
        serde_json::from_value(
            serde_json::json!({"instance_id": "default__web-0", "generation": "a".repeat(64)}),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn producer_client_requires_exact_identity_execution_and_positive_confirmation() {
        use axum::http::StatusCode;
        let original = execution();
        let valid = serde_json::json!({"node_id": "producer", "execution": original}).to_string();
        let (client, task) = fixture(StatusCode::OK, valid.clone()).await;
        assert_eq!(
            client
                .confirm("producer", &original)
                .await
                .unwrap()
                .execution,
            original
        );
        assert!(client.confirm("another", &original).await.is_err());
        let mut newer = original.clone();
        newer.generation = "b".repeat(64).try_into().unwrap();
        assert!(client.confirm("producer", &newer).await.is_err());
        let mut plaintext = client;
        plaintext.allow_plaintext = false;
        assert!(plaintext.confirm("producer", &original).await.is_err());
        task.abort();
        let _ = task.await;
        for (status, body) in [
            (StatusCode::ACCEPTED, valid),
            (StatusCode::NO_CONTENT, String::new()),
            (StatusCode::OK, "{}".into()),
            (StatusCode::OK, "x".repeat(17_000)),
            (StatusCode::SERVICE_UNAVAILABLE, String::new()),
        ] {
            let (client, task) = fixture(status, body).await;
            assert!(client.confirm("producer", &original).await.is_err());
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
pub(crate) use tests::{delayed_fixture as test_delayed_fixture, fixture as test_fixture};

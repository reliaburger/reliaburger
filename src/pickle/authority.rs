//! Authenticated proposals to the current registry catalogue leader.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::types::{GcReport, ManifestCommit, PickleError};
use crate::cluster::ClusterHttp;
use crate::council::{CouncilNode, CouncilResponse, RaftRequest};
use crate::mustard::directory::NodeDirectory;

/// Restricted control endpoint; it never accepts arbitrary Raft requests.
pub const REGISTRY_PROPOSAL_PATH: &str = "/v1/registry/propose";
/// Bound both incoming proposals and outgoing arbitration responses.
pub const MAX_REGISTRY_PROPOSAL_BYTES: usize = 8 * 1024 * 1024;
const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Registry operations that a storage node may propose for its own holdings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegistryMutation {
    /// Publish a manifest whose blobs are stored on the authenticated node.
    Manifest(Box<ManifestCommit>),
    /// Ask to remove only the authenticated node's blob holdings.
    GarbageCollection(GcReport),
}

impl RegistryMutation {
    /// Refuse claims about another node before constructing a Raft operation.
    pub fn request_for_node(&self, node_name: &str) -> Result<RaftRequest, PickleError> {
        let id = crate::cluster::identity::raft_id_from_name(node_name);
        let valid = match self {
            Self::Manifest(commit) => {
                commit.holder_nodes == std::collections::BTreeSet::from([id])
                    && commit.manifest.pushed_by == id
            }
            Self::GarbageCollection(report) => report.node_id == id,
        };
        if !valid {
            return Err(PickleError::ReplicationFailed(
                "registry proposal does not belong to the authenticated node".into(),
            ));
        }
        Ok(self.request())
    }

    pub(crate) fn request(&self) -> RaftRequest {
        match self {
            Self::Manifest(commit) => RaftRequest::ManifestCommit(commit.as_ref().clone()),
            Self::GarbageCollection(report) => RaftRequest::GcReport(report.clone()),
        }
    }
}

/// A versioned proposal from one authenticated node to the current leader.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryProposal {
    /// Required explicit protocol and durable-state generations.
    pub compatibility: crate::compatibility::Compatibility,
    /// The restricted registry operation being proposed.
    pub mutation: RegistryMutation,
}

/// Resolve the authoritative leader even on a worker without a council handle.
#[derive(Clone)]
pub struct RegistryForwarder {
    http: ClusterHttp,
    directory: watch::Receiver<NodeDirectory>,
}

impl RegistryForwarder {
    /// Use the cluster's credentialled, non-redirecting client and live directory.
    pub fn new(http: ClusterHttp, directory: watch::Receiver<NodeDirectory>) -> Self {
        Self { http, directory }
    }

    /// Commit locally when possible, otherwise send directly to the leader.
    /// A timeout is uncertain and never authorises local-only success.
    pub async fn write(
        &self,
        council: Option<&Arc<CouncilNode>>,
        mutation: RegistryMutation,
    ) -> Result<CouncilResponse, PickleError> {
        tokio::time::timeout(PROPOSAL_TIMEOUT, self.write_inner(council, mutation))
            .await
            .map_err(|_| {
                unavailable("registry proposal timed out; retry to establish acceptance")
            })?
    }

    async fn write_inner(
        &self,
        council: Option<&Arc<CouncilNode>>,
        mutation: RegistryMutation,
    ) -> Result<CouncilResponse, PickleError> {
        if let Some(council) = council {
            match council.write(mutation.request()).await {
                Ok(response) => return Ok(response),
                Err(crate::council::CouncilError::ForwardToLeader { .. }) => {}
                Err(error) => return Err(unavailable(error.to_string())),
            }
        }
        let address = {
            let directory = self.directory.borrow();
            if let Some(council) = council {
                let metrics = council.metrics();
                let metrics = metrics.borrow();
                crate::cluster::directory::resolve_leader(&metrics, &directory, 0, 0)
                    .and_then(|leader| {
                        directory
                            .endpoints
                            .get(&leader.node_id)
                            .map(|node| node.api_address)
                            .or_else(|| {
                                directory
                                    .leader
                                    .as_ref()
                                    .filter(|hint| {
                                        hint.node_id == leader.node_id && hint.term == leader.term
                                    })
                                    .map(|hint| hint.api_address)
                            })
                    })
                    .or_else(|| {
                        // A fresh worker may not have learned any Raft term yet.
                        // This route hint never advances consensus or a reporting
                        // epoch: the destination must independently prove quorum.
                        directory.leader.as_ref().map(|leader| leader.api_address)
                    })
            } else {
                directory.leader.as_ref().map(|leader| leader.api_address)
            }
        }
        .ok_or_else(|| unavailable("no advertised registry leader endpoint"))?;
        let bearer = self
            .http
            .bearer()
            .ok_or_else(|| unavailable("registry forwarding requires a service credential"))?;
        let url = self.http.url(&address.to_string(), REGISTRY_PROPOSAL_PATH);
        let proposal = RegistryProposal {
            compatibility: crate::compatibility::CURRENT,
            mutation,
        };
        let bytes =
            serde_json::to_vec(&proposal).map_err(|error| unavailable(error.to_string()))?;
        if bytes.len() > MAX_REGISTRY_PROPOSAL_BYTES {
            return Err(unavailable(
                "registry proposal exceeds the control-message limit",
            ));
        }
        let mut response = self
            .http
            .client()
            .post(&url)
            .bearer_auth(bearer)
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|error| unavailable(error.to_string()))?;
        if response.url().as_str() != url || !response.status().is_success() {
            return Err(unavailable(format!(
                "registry leader refused proposal: {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_REGISTRY_PROPOSAL_BYTES as u64)
        {
            return Err(unavailable(
                "registry response exceeds the control-message limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| unavailable(error.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_REGISTRY_PROPOSAL_BYTES {
                return Err(unavailable(
                    "registry response exceeds the control-message limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| unavailable(format!("invalid registry response: {error}")))
    }
}

fn unavailable(message: impl Into<String>) -> PickleError {
    PickleError::ReplicationFailed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::council::CouncilResponse;
    use crate::mustard::directory::NodeDirectory;
    use crate::mustard::message::LeaderHint;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use tokio::sync::watch;

    fn report() -> RegistryMutation {
        RegistryMutation::GarbageCollection(super::super::types::GcReport {
            node_id: crate::cluster::identity::raft_id_from_name("writer"),
            deleted_layers: vec![super::super::store::compute_sha256(b"orphan")],
        })
    }

    fn directory(address: std::net::SocketAddr) -> NodeDirectory {
        NodeDirectory {
            leader: Some(LeaderHint {
                node_id: crate::meat::NodeId::new("leader"),
                api_address: address,
                reporting_address: address,
                term: 1,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn forwarding_carries_service_authority_and_follows_updated_directory() {
        async fn accept(
            headers: HeaderMap,
            Json(request): Json<RegistryProposal>,
        ) -> Json<CouncilResponse> {
            assert_eq!(headers["authorization"], "Bearer internal");
            request.compatibility.require_current().unwrap();
            assert!(matches!(
                request.mutation,
                RegistryMutation::GarbageCollection(_)
            ));
            Json(CouncilResponse::GcApproved { approved: vec![] })
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(REGISTRY_PROPOSAL_PATH, post(accept)),
            )
            .await
            .unwrap();
        });
        let (tx, rx) = watch::channel(NodeDirectory::default());
        let forwarder = RegistryForwarder::new(
            crate::cluster::ClusterHttp::plaintext().with_bearer(Some("internal".into())),
            rx,
        );
        assert!(forwarder.write(None, report()).await.is_err());
        tx.send(directory(address)).unwrap();
        assert_eq!(
            forwarder.write(None, report()).await.unwrap(),
            CouncilResponse::GcApproved { approved: vec![] }
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn forwarding_does_not_accept_refusal_or_redirect_as_a_commit() {
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TEMPORARY_REDIRECT,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let followed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let captured = followed.clone();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new()
                        .route(
                            REGISTRY_PROPOSAL_PATH,
                            post(move || async move { (status, [("location", "/unexpected")]) }),
                        )
                        .route(
                            "/unexpected",
                            post(move || async move {
                                captured.store(true, std::sync::atomic::Ordering::SeqCst);
                                Json(CouncilResponse::Ok)
                            }),
                        ),
                )
                .await
                .unwrap();
            });
            let (_tx, rx) = watch::channel(directory(address));
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let forwarder = RegistryForwarder::new(
                crate::cluster::ClusterHttp::plaintext_with_client(client)
                    .with_bearer(Some("internal".into())),
                rx,
            );
            assert!(forwarder.write(None, report()).await.is_err());
            assert!(!followed.load(std::sync::atomic::Ordering::SeqCst));
            task.abort();
            let _ = task.await;
        }
    }

    #[tokio::test]
    async fn forwarding_bounds_chunked_responses_and_the_whole_deadline() {
        use axum::{body::Body, response::Response};
        use std::sync::atomic::{AtomicBool, Ordering};
        let hanging = Arc::new(AtomicBool::new(false));
        let mode = hanging.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    REGISTRY_PROPOSAL_PATH,
                    post(move || {
                        let mode = mode.clone();
                        async move {
                            let body = if mode.load(Ordering::SeqCst) {
                                Body::from_stream(futures_util::stream::pending::<
                                    Result<axum::body::Bytes, std::io::Error>,
                                >())
                            } else {
                                Body::from_stream(futures_util::stream::iter((0..=1024).map(
                                    |_| {
                                        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![
                                            b'x';
                                            8192
                                        ]))
                                    },
                                )))
                            };
                            Response::new(body)
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let (_tx, rx) = watch::channel(directory(address));
        let forwarder = RegistryForwarder::new(
            crate::cluster::ClusterHttp::plaintext().with_bearer(Some("internal".into())),
            rx,
        );
        assert!(
            forwarder
                .write(None, report())
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        hanging.store(true, Ordering::SeqCst);
        assert!(
            forwarder
                .write(None, report())
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn proposals_cannot_claim_another_nodes_blob_holdings() {
        assert!(report().request_for_node("writer").is_ok());
        assert!(report().request_for_node("another").is_err());
        let manifest = super::super::types::ImageManifest {
            repository: "ordinary".into(),
            tags: std::collections::BTreeSet::from(["latest".into()]),
            digest: super::super::store::compute_sha256(b"manifest"),
            config: super::super::types::LayerDescriptor {
                digest: super::super::store::compute_sha256(b"config"),
                size: 6,
                media_type: "application/vnd.oci.image.config.v1+json".into(),
            },
            pushed_by: crate::cluster::identity::raft_id_from_name("writer"),
            layers: vec![],
            total_size: 14,
            pushed_at: std::time::SystemTime::now(),
            signature: None,
        };
        let mutation = RegistryMutation::Manifest(Box::new(super::super::types::ManifestCommit {
            manifest,
            tag: "latest".into(),
            holder_nodes: std::collections::BTreeSet::from([
                crate::cluster::identity::raft_id_from_name("writer"),
                crate::cluster::identity::raft_id_from_name("another"),
            ]),
        }));
        assert!(mutation.request_for_node("writer").is_err());
    }
}

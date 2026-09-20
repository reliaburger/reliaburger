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
    /// Confirm the authenticated node's verified copy of a committed image.
    Copy(super::types::ImageCopyConfirmation),
    /// Publish a manifest whose blobs are stored on the authenticated node.
    Manifest(Box<ManifestCommit>),
    /// Ask to remove only the authenticated node's blob holdings.
    GarbageCollection(GcReport),
    /// Record this storage node before accepting bytes for an active lease.
    ClaimWriter {
        lease_id: String,
        repository: String,
        node_id: u64,
        owner_id: Option<String>,
        observed_at_unix_ms: u64,
    },
    /// Publish only while the recorded lease and its writer remain active.
    LeasedManifest {
        lease_id: String,
        observed_at_unix_ms: u64,
        commit: Box<ManifestCommit>,
    },
    /// Confirm this node's exact repository obligation after local retirement.
    WriterRetired {
        lease_id: String,
        repository: String,
        node_id: u64,
    },
}

impl RegistryMutation {
    /// Refuse claims about another node before constructing a Raft operation.
    pub fn request_for_node(&self, node_name: &str) -> Result<RaftRequest, PickleError> {
        let id = crate::cluster::identity::raft_id_from_name(node_name);
        let valid = match self {
            Self::Copy(copy) => copy.node_id == id,
            Self::Manifest(commit) | Self::LeasedManifest { commit, .. } => {
                commit.holder_nodes == std::collections::BTreeSet::from([id])
                    && commit.manifest.pushed_by == id
            }
            Self::GarbageCollection(report) => report.node_id == id,
            Self::ClaimWriter { node_id, .. } | Self::WriterRetired { node_id, .. } => {
                *node_id == id
            }
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
            Self::Copy(copy) => RaftRequest::ConfirmImageCopy(copy.clone()),
            Self::Manifest(commit) => RaftRequest::ManifestCommit(commit.as_ref().clone()),
            Self::GarbageCollection(report) => RaftRequest::GcReport(report.clone()),
            Self::ClaimWriter {
                lease_id,
                repository,
                node_id,
                owner_id,
                observed_at_unix_ms,
            } => RaftRequest::TestLeaseRegistryWriter {
                lease_id: lease_id.clone(),
                repository: repository.clone(),
                node_id: *node_id,
                owner_id: owner_id.clone(),
                observed_at_unix_ms: *observed_at_unix_ms,
            },
            Self::LeasedManifest {
                lease_id,
                observed_at_unix_ms,
                commit,
            } => RaftRequest::TestLeaseManifestCommit {
                lease_id: lease_id.clone(),
                observed_at_unix_ms: *observed_at_unix_ms,
                commit: commit.clone(),
            },
            Self::WriterRetired {
                lease_id,
                repository,
                node_id,
            } => RaftRequest::TestLeaseRegistryRetired {
                lease_id: lease_id.clone(),
                repository: repository.clone(),
                node_id: *node_id,
            },
        }
    }
}

/// Node-local routing identity for authenticated public catalogue reads.
#[derive(Clone)]
pub struct RegistryReadAuthority {
    /// Current-leader transport with this node's live TLS credentials.
    pub forwarder: RegistryForwarder,
    /// The node's own immutable cluster identifier.
    pub node_id: u64,
}

/// Restricted registry queries whose answers require current council authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegistryQuery {
    /// Read the authenticated node's current publication fencing generation.
    GcGeneration,
    /// List public image metadata from the committed catalogue.
    Images,
    /// Read committed metadata needed to resolve and copy one repository's images.
    Repository { repository: String },
    /// Read current logical usage without transferring the whole catalogue.
    Usage { repository: String },
    /// Find the active lease which already owns this repository.
    Lease { repository: String },
    /// Find repository retirement obligations belonging to the authenticated node.
    Retirements,
}

/// One storage node's outstanding repository cleanup obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRetirement {
    /// Exact lease generation, retained through cleanup.
    pub lease_id: String,
    /// Repository whose uploads and local metadata must retire.
    pub repository: String,
}

/// Versioned read request bound to the sending node's TLS identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryQueryRequest {
    /// Explicit supported wire and state generations.
    pub compatibility: crate::compatibility::Compatibility,
    /// Node requesting its own receipt inventory.
    pub node_id: u64,
    /// The requested bounded registry view.
    pub query: RegistryQuery,
}

/// A current registry ownership view; this never exposes other leases' credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegistryQueryResponse {
    /// Current fencing generation for this node's physical collection.
    GcGeneration(u64),
    /// Public image rows; no lease credentials or internal storage receipts.
    Images(Vec<super::types::ImageSummary>),
    /// A repository-scoped current catalogue, including its shared holder records.
    Repository(Box<super::types::ManifestCatalog>),
    /// Logical image bytes in this repository and in the complete registry.
    Usage {
        repository_bytes: u64,
        total_bytes: u64,
    },
    /// The active repository lease, or no active owner.
    Lease(Option<String>),
    /// Workload-retired repositories still awaiting this node's confirmation.
    Retirements(Vec<RegistryRetirement>),
}

/// Internal query endpoint, guarded like registry proposals.
pub const REGISTRY_QUERY_PATH: &str = "/v1/registry/query";

impl RegistryQuery {
    /// Select only committed ownership after the caller establishes a current view.
    pub fn answer(
        &self,
        state: &crate::council::DesiredState,
        node_id: u64,
    ) -> RegistryQueryResponse {
        match self {
            Self::GcGeneration => RegistryQueryResponse::GcGeneration(
                state
                    .registry_gc_generations
                    .get(&node_id)
                    .copied()
                    .unwrap_or(0),
            ),
            Self::Images => RegistryQueryResponse::Images(state.manifest_catalog.images()),
            Self::Repository { repository } => RegistryQueryResponse::Repository(Box::new(
                state.manifest_catalog.repository_view(repository),
            )),
            Self::Usage { repository } => {
                let (repository_bytes, total_bytes) =
                    state.manifest_catalog.stored_sizes(repository);
                RegistryQueryResponse::Usage {
                    repository_bytes,
                    total_bytes,
                }
            }
            Self::Lease { repository } => RegistryQueryResponse::Lease(
                state
                    .test_leases
                    .values()
                    .find(|lease| {
                        lease.is_active_at(crate::testkit::lease::now_unix_millis())
                            && lease.repositories.contains_key(repository)
                    })
                    .map(|lease| lease.lease_id.clone()),
            ),
            Self::Retirements => RegistryQueryResponse::Retirements(
                state
                    .test_leases
                    .values()
                    .filter(|lease| {
                        lease.workloads_retired
                            && matches!(
                                lease.state,
                                crate::testkit::lease::TestLeaseState::Cleaning { .. }
                            )
                    })
                    .flat_map(|lease| {
                        lease
                            .repositories
                            .iter()
                            .filter(move |(_, owners)| owners.contains(&node_id))
                            .map(|(repository, _)| RegistryRetirement {
                                lease_id: lease.lease_id.clone(),
                                repository: repository.clone(),
                            })
                    })
                    .collect(),
            ),
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
        let proposal = RegistryProposal {
            compatibility: crate::compatibility::CURRENT,
            mutation,
        };
        self.send_request(council, REGISTRY_PROPOSAL_PATH, &proposal)
            .await
    }

    /// Read current ownership directly from the advertised authoritative node.
    pub async fn query(
        &self,
        council: Option<&Arc<CouncilNode>>,
        node_id: u64,
        query: RegistryQuery,
    ) -> Result<RegistryQueryResponse, PickleError> {
        let request = RegistryQueryRequest {
            compatibility: crate::compatibility::CURRENT,
            node_id,
            query,
        };
        tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.send_request(council, REGISTRY_QUERY_PATH, &request),
        )
        .await
        .map_err(|_| unavailable("registry ownership query timed out"))?
    }

    async fn send_request<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        council: Option<&Arc<CouncilNode>>,
        path: &str,
        request: &T,
    ) -> Result<R, PickleError> {
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
        let url = self.http.url(&address.to_string(), path);
        let bytes = serde_json::to_vec(request).map_err(|error| unavailable(error.to_string()))?;
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
        if response.url().as_str() != url {
            return Err(unavailable("registry forwarding refused a redirect"));
        }
        let status = response.status();
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
        if !status.is_success() {
            if status == reqwest::StatusCode::CONFLICT
                && let Ok(CouncilResponse::Refused { reason }) = serde_json::from_slice(&bytes)
            {
                return Err(PickleError::LeaseDenied(reason));
            }
            return Err(unavailable(format!(
                "registry leader refused proposal: {status}"
            )));
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
            observed_gc_generation: 0,
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

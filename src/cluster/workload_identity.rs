//! Workload certificate signing, forwarded from followers to the leader.
//!
//! Signing a workload certificate reads the CA from linearised state and
//! allocates a serial through Raft, so only the leader can do it. Before this
//! existed every follower's `sign_workload_csr` failed quietly and its
//! containers started without an identity.

use crate::council::types::{CouncilNodeInfo, DesiredState};
use crate::mustard::directory::NodeDirectory;
use crate::sesame::types::WorkloadType;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use std::io;
use tokio::sync::watch;

/// A follower's request to sign a CSR for one of its instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadCsrRequest {
    /// Required wire and state format compatibility.
    pub compatibility: crate::compatibility::Compatibility,
    /// The instance the certificate is for, such as `default__web-0`.
    pub instance_id: String,
    /// App or job.
    pub workload_type: WorkloadType,
    /// Base64 DER-encoded CSR. The private key never leaves the node.
    pub csr_der: String,
}

/// The leader's signed certificate and the chain to install beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadCsrResponse {
    /// Base64 DER-encoded workload certificate.
    pub cert_der: String,
    /// Base64 DER-encoded Workload CA certificate.
    pub workload_ca_cert_der: String,
    /// Base64 DER-encoded Root CA certificate.
    pub root_ca_cert_der: String,
    /// OIDC JWT, when the cluster issues them.
    pub jwt_token: Option<String>,
}

/// A signed workload certificate, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedWorkload {
    /// DER-encoded workload certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded Workload CA certificate.
    pub workload_ca_cert_der: Vec<u8>,
    /// DER-encoded Root CA certificate.
    pub root_ca_cert_der: Vec<u8>,
    /// OIDC JWT, when the cluster issues them.
    pub jwt_token: Option<String>,
}

impl WorkloadCsrResponse {
    /// Encode a signed certificate for the wire.
    pub fn encode(signed: &SignedWorkload) -> Self {
        Self {
            cert_der: BASE64.encode(&signed.cert_der),
            workload_ca_cert_der: BASE64.encode(&signed.workload_ca_cert_der),
            root_ca_cert_der: BASE64.encode(&signed.root_ca_cert_der),
            jwt_token: signed.jwt_token.clone(),
        }
    }

    /// Decode the wire form, refusing malformed base64.
    pub fn decode(self) -> io::Result<SignedWorkload> {
        let decode = |field: &str, value: &str| {
            BASE64
                .decode(value)
                .map_err(|e| io::Error::other(format!("workload {field} is not base64: {e}")))
        };
        Ok(SignedWorkload {
            cert_der: decode("certificate", &self.cert_der)?,
            workload_ca_cert_der: decode("Workload CA", &self.workload_ca_cert_der)?,
            root_ca_cert_der: decode("Root CA", &self.root_ca_cert_der)?,
            jwt_token: self.jwt_token,
        })
    }
}

/// Decide whether `node_id` may have a certificate for `instance_id`, and
/// for which namespace and name.
///
/// The SPIFFE identity comes from the instance id, never from the caller, and
/// an app's instance must be scheduled on the asking node. Jobs aren't
/// scheduled through placements, so a job only has to exist.
pub fn authorise(
    desired: &DesiredState,
    node_id: &str,
    instance_id: &str,
    workload_type: WorkloadType,
) -> Result<(String, String), String> {
    let identity = crate::grill::InstanceIdentity::parse(instance_id)
        .ok_or_else(|| format!("invalid instance id {instance_id:?}"))?;
    let app_id = crate::meat::types::AppId::new(&identity.app, &identity.namespace);
    match workload_type {
        WorkloadType::App => {
            let placed = desired.scheduling.get(&app_id).is_some_and(|placements| {
                placements
                    .iter()
                    .any(|placement| placement.node_id.0 == node_id)
            });
            if !placed {
                return Err(format!(
                    "{}/{} is not scheduled on node {node_id}",
                    identity.namespace, identity.app
                ));
            }
        }
        WorkloadType::Job => {
            if !desired.apps.contains_key(&app_id) {
                return Err(format!(
                    "job {}/{} does not exist",
                    identity.namespace, identity.app
                ));
            }
        }
    }
    Ok((identity.namespace, identity.app))
}

/// Transport and live leader information for workload CSR signing.
#[derive(Clone)]
pub struct WorkloadCsrClient {
    http: super::ClusterHttp,
    metrics: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    directory: watch::Receiver<NodeDirectory>,
    raft_to_api_offset: i32,
}

impl WorkloadCsrClient {
    /// Use the enrolled node's mTLS client: the leader identifies the node by it.
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
        }
    }

    /// Ask the leader to sign `csr_der` for `instance_id`.
    pub async fn sign(
        &self,
        instance_id: &str,
        workload_type: WorkloadType,
        csr_der: &[u8],
    ) -> io::Result<SignedWorkload> {
        if self.http.scheme() != "https" {
            return Err(io::Error::other(
                "workload signing requires authenticated HTTPS",
            ));
        }
        let address = super::directory::resolve_leader(
            &self.metrics.borrow(),
            &self.directory.borrow(),
            self.raft_to_api_offset,
            0,
        )
        .and_then(|leader| leader.api_address)
        .ok_or_else(|| io::Error::other("workload signing has no known leader"))?;
        let url = self
            .http
            .url(&address.to_string(), "/v1/cluster/workload-csr");
        let request = WorkloadCsrRequest {
            compatibility: crate::compatibility::CURRENT,
            instance_id: instance_id.to_string(),
            workload_type,
            csr_der: BASE64.encode(csr_der),
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut request = self.http.client().post(&url).json(&request);
            if let Some(token) = self.http.bearer() {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(io::Error::other)?;
            if response.url().as_str() != url || !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(io::Error::other(format!(
                    "leader refused workload signing ({status}): {body}"
                )));
            }
            let body: WorkloadCsrResponse = response.json().await.map_err(io::Error::other)?;
            body.decode()
        })
        .await
        .map_err(|_| io::Error::other("workload signing timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::types::{AppId, NodeId, Placement};

    fn desired_with(app: &str, node: &str) -> DesiredState {
        let mut desired = DesiredState::default();
        let app_id = AppId::new(app, "default");
        let spec: crate::config::app::AppSpec =
            toml::from_str("image = \"busybox:1\"").expect("minimal app spec");
        desired.apps.insert(app_id.clone(), spec);
        desired.scheduling.insert(
            app_id,
            vec![Placement {
                node_id: NodeId::new(node),
                resources: Default::default(),
            }],
        );
        desired
    }

    #[test]
    fn an_app_scheduled_on_the_asking_node_is_signed_for_its_own_identity() {
        let desired = desired_with("web", "node-2");
        assert_eq!(
            authorise(&desired, "node-2", "default__web-0", WorkloadType::App),
            Ok(("default".to_string(), "web".to_string()))
        );
    }

    #[test]
    fn a_node_cannot_have_a_certificate_for_an_app_scheduled_elsewhere() {
        let desired = desired_with("web", "node-2");
        assert!(authorise(&desired, "node-3", "default__web-0", WorkloadType::App).is_err());
        assert!(authorise(&desired, "node-2", "default__db-0", WorkloadType::App).is_err());
        assert!(authorise(&desired, "node-2", "not-an-instance", WorkloadType::App).is_err());
    }

    #[test]
    fn a_job_only_has_to_exist() {
        let desired = desired_with("batch", "node-1");
        assert!(authorise(&desired, "node-3", "default__batch-0", WorkloadType::Job).is_ok());
        assert!(authorise(&desired, "node-3", "default__other-0", WorkloadType::Job).is_err());
    }

    #[test]
    fn the_wire_form_round_trips_and_refuses_bad_base64() {
        let signed = SignedWorkload {
            cert_der: vec![1, 2, 3],
            workload_ca_cert_der: vec![4],
            root_ca_cert_der: vec![5, 6],
            jwt_token: Some("jwt".into()),
        };
        assert_eq!(
            WorkloadCsrResponse::encode(&signed).decode().unwrap(),
            signed
        );
        let mut bad = WorkloadCsrResponse::encode(&signed);
        bad.cert_der = "!!!".into();
        assert!(bad.decode().is_err());
    }
}

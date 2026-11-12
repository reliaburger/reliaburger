//! What the gym asks of a cluster, and the real one behind it.
//!
//! The runner talks to the cluster only through [`GymCluster`], so the unit
//! tests can drive every exit path (a refusal, a crash, a panic, Ctrl-C)
//! against a fake that records each call, and check that cleanup undid
//! everything. The real implementation is a thin layer over [`BunClient`],
//! the same client every other `relish` command uses, with the user's own
//! credentials.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::bun::audit::AuditFilter;
use crate::relish::RelishError;
use crate::relish::client::{BunClient, TokenLifetime, TokenRequest};
use crate::smoker::types::{FaultRequest, FaultSummary};

/// What the gym needs to know about the cluster before it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterFacts {
    /// Its name.
    pub cluster_name: String,
    /// The container runtime (`process` for ProcessGrill).
    pub runtime: String,
    /// Its `environment` tag, if any.
    pub environment: Option<String>,
    /// The version the answering node runs.
    pub version: String,
}

impl ClusterFacts {
    /// Whether the cluster is tagged as production.
    pub fn is_production(&self) -> bool {
        self.environment
            .as_deref()
            .is_some_and(|environment| environment.eq_ignore_ascii_case("production"))
    }
}

/// One instance of an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The node it runs on.
    pub node: String,
    /// Its id.
    pub id: String,
    /// Its state, lowercase (`running`, `pending`...).
    pub state: String,
}

/// A fault the cluster holds: where, and what it targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlacedFault {
    /// The fault id, per node.
    pub id: u64,
    /// The node that holds it.
    pub node: Option<String>,
    /// The app it targets.
    pub app: String,
    /// Its namespace.
    pub namespace: Option<String>,
    /// The kind, as the cluster describes it (`pause`, `kill 1`).
    pub kind: String,
}

impl PlacedFault {
    fn from_summary(summary: &FaultSummary) -> Self {
        Self {
            id: summary.id,
            node: summary.node.clone().or_else(|| summary.target_node.clone()),
            app: summary.target_service.clone(),
            namespace: summary.namespace.clone(),
            kind: summary.fault_type.clone(),
        }
    }

    /// Every fault an injection answer names: the first and each routed one.
    pub fn all_from(summary: &FaultSummary) -> Vec<Self> {
        std::iter::once(summary)
            .chain(summary.routed.iter())
            .map(Self::from_summary)
            .collect()
    }
}

/// What the solver needs to reach the cluster: the user's endpoint and CA,
/// never the user's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The API's base URL.
    pub endpoint: String,
    /// The cluster CA, PEM, when the endpoint is HTTPS.
    pub ca_pem: Option<Vec<u8>>,
}

/// A failure talking to the cluster.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ClusterError(pub String);

impl From<RelishError> for ClusterError {
    fn from(error: RelishError) -> Self {
        ClusterError(error.to_string())
    }
}

/// What the gym does to a cluster. Every method that changes something
/// names the namespace, and none can reach outside it.
///
/// `async fn` in a trait is stable since Rust 1.75; it's used here through
/// generics only (`impl GymCluster`), never as `dyn GymCluster`, which async
/// methods don't allow.
#[allow(async_fn_in_trait)]
pub trait GymCluster {
    /// The cluster's name, runtime and environment tag.
    async fn facts(&self) -> Result<ClusterFacts, ClusterError>;
    /// The name of every app and job in `namespace`, desired or running.
    async fn workloads(&self, namespace: &str) -> Result<Vec<String>, ClusterError>;
    /// Apply one app manifest.
    async fn apply(&self, manifest: &str) -> Result<(), ClusterError>;
    /// The instances of `app` in `namespace`.
    async fn instances(&self, namespace: &str, app: &str) -> Result<Vec<Instance>, ClusterError>;
    /// Delete `app` from `namespace`.
    async fn delete(&self, namespace: &str, app: &str) -> Result<(), ClusterError>;
    /// Inject a workload fault; the answer names every fault it placed.
    async fn inject(&self, request: &FaultRequest) -> Result<Vec<PlacedFault>, ClusterError>;
    /// Clear one fault by id, on its node.
    async fn clear(&self, fault: &PlacedFault) -> Result<(), ClusterError>;
    /// Clear every fault on `app` in `namespace`.
    async fn clear_app(&self, namespace: &str, app: &str) -> Result<(), ClusterError>;
    /// Every active fault on the cluster.
    async fn active_faults(&self) -> Result<Vec<PlacedFault>, ClusterError>;
    /// Create a read-only token confined to `namespace`, for one day.
    async fn create_token(&self, name: &str, namespace: &str) -> Result<String, ClusterError>;
    /// Revoke a token.
    async fn revoke_token(&self, name: &str) -> Result<(), ClusterError>;
    /// Every audit record, on every node, under the token named.
    async fn audit(&self, token_name: &str) -> Result<Vec<serde_json::Value>, ClusterError>;
    /// How the solver reaches the cluster.
    fn connection(&self) -> Connection;
}

/// The real cluster, through the user's own client.
pub struct LiveCluster {
    client: BunClient,
}

impl LiveCluster {
    /// The cluster `relish` would talk to for any other command.
    pub fn new(client: BunClient) -> Self {
        Self { client }
    }
}

/// The token lifetime the solver gets: one day, so a token cleanup missed
/// still stops by itself.
const SOLVER_TOKEN_DAYS: u64 = 1;

/// Audit records to read per run: far more than a solver makes.
const AUDIT_LIMIT: usize = 10_000;

impl GymCluster for LiveCluster {
    async fn facts(&self) -> Result<ClusterFacts, ClusterError> {
        let capabilities = self.client.capabilities().await?;
        Ok(ClusterFacts {
            cluster_name: capabilities.cluster_name,
            runtime: capabilities.container_runtime,
            environment: capabilities.environment,
            version: capabilities.version,
        })
    }

    async fn workloads(&self, namespace: &str) -> Result<Vec<String>, ClusterError> {
        let mut names: Vec<String> = self
            .client
            .desired_apps()
            .await?
            .into_iter()
            .filter(|app| app.namespace == namespace)
            .map(|app| app.app)
            .collect();
        names.extend(
            self.client
                .cluster_status()
                .await?
                .into_iter()
                .filter(|row| row.instance.namespace == namespace)
                .map(|row| row.instance.app_name),
        );
        names.extend(
            self.client
                .jobs()
                .await?
                .jobs
                .into_iter()
                .filter(|job| job.row.namespace == namespace)
                .map(|job| job.row.name),
        );
        names.sort();
        names.dedup();
        Ok(names)
    }

    async fn apply(&self, manifest: &str) -> Result<(), ClusterError> {
        let config = crate::config::Config::parse(manifest)
            .map_err(|error| ClusterError(error.to_string()))?;
        self.client.apply(&config).await?;
        Ok(())
    }

    async fn instances(&self, namespace: &str, app: &str) -> Result<Vec<Instance>, ClusterError> {
        Ok(self
            .client
            .cluster_status()
            .await?
            .into_iter()
            .filter(|row| row.instance.namespace == namespace && row.instance.app_name == app)
            .map(|row| Instance {
                node: row.node,
                id: row.instance.id,
                state: row.instance.state.to_lowercase(),
            })
            .collect())
    }

    async fn delete(&self, namespace: &str, app: &str) -> Result<(), ClusterError> {
        Ok(self.client.delete(app, namespace).await?)
    }

    async fn inject(&self, request: &FaultRequest) -> Result<Vec<PlacedFault>, ClusterError> {
        let summary = self.client.inject_fault(request).await?;
        Ok(PlacedFault::all_from(&summary))
    }

    async fn clear(&self, fault: &PlacedFault) -> Result<(), ClusterError> {
        self.client
            .clear_fault(fault.id, fault.node.as_deref(), true)
            .await?;
        Ok(())
    }

    async fn clear_app(&self, namespace: &str, app: &str) -> Result<(), ClusterError> {
        self.client
            .clear_faults_by_service(app, Some(namespace))
            .await?;
        Ok(())
    }

    async fn active_faults(&self) -> Result<Vec<PlacedFault>, ClusterError> {
        let listing = self.client.list_cluster_faults().await?;
        if !listing.warnings.is_empty() {
            // A node whose faults couldn't be read might hold one of ours,
            // so an incomplete listing proves nothing.
            return Err(ClusterError(format!(
                "the fault listing is incomplete: {}",
                listing.warnings.join("; ")
            )));
        }
        Ok(listing
            .faults
            .iter()
            .map(PlacedFault::from_summary)
            .collect())
    }

    async fn create_token(&self, name: &str, namespace: &str) -> Result<String, ClusterError> {
        let issued = self
            .client
            .token_create(&TokenRequest {
                name: name.to_string(),
                role: "read-only".to_string(),
                apps: None,
                namespaces: Some(vec![namespace.to_string()]),
                lifetime: TokenLifetime::Days(SOLVER_TOKEN_DAYS),
                inherit_permissions: false,
                agent: false,
            })
            .await?;
        Ok(issued.token)
    }

    async fn revoke_token(&self, name: &str) -> Result<(), ClusterError> {
        self.client.token_revoke(name).await?;
        Ok(())
    }

    async fn audit(&self, token_name: &str) -> Result<Vec<serde_json::Value>, ClusterError> {
        let filter = AuditFilter {
            principal: Some(token_name.to_string()),
            limit: Some(AUDIT_LIMIT),
            ..AuditFilter::default()
        };
        let listing = self.client.audit(&filter, true).await?;
        if !listing.warnings.is_empty() {
            return Err(ClusterError(format!(
                "the audit log is incomplete: {}",
                listing.warnings.join("; ")
            )));
        }
        listing
            .records
            .iter()
            .map(|record| {
                serde_json::to_value(record).map_err(|error| ClusterError(error.to_string()))
            })
            .collect()
    }

    fn connection(&self) -> Connection {
        Connection {
            endpoint: self.client.base_url().to_string(),
            ca_pem: self.client.ca_pem().map(<[u8]>::to_vec),
        }
    }
}

/// The node each app's first instance runs on, for the truth.
pub async fn instance_nodes(
    cluster: &impl GymCluster,
    namespace: &str,
    apps: &[&str],
) -> Result<BTreeMap<String, String>, ClusterError> {
    let mut nodes = BTreeMap::new();
    for app in apps {
        let mut instances = cluster.instances(namespace, app).await?;
        instances.sort_by(|left, right| left.node.cmp(&right.node));
        if let Some(first) = instances.first() {
            nodes.insert(app.to_string(), first.node.clone());
        }
    }
    Ok(nodes)
}

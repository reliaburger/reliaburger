//! Workload identity signing that runs off the command loop.
//!
//! Only the leader can sign a workload certificate. A follower sends its CSR
//! to the leader over HTTP, which can take up to ten seconds, and even the
//! leader's own signing starts with a linearizable Raft read. Awaiting either
//! inline held every queued command for as long as it took. The loop now
//! generates the CSR (the private key never leaves this node), hands the
//! signing to a task in `identity_signing_tasks`, and writes the files and
//! records the identity when that task reports back. Every change to agent
//! state still happens on the loop, one at a time.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;

use super::{BunAgent, Grill, InstanceId};
use crate::cluster::workload_identity::{SignedWorkload, WorkloadCsrClient};
use crate::council::node::CouncilNode;
use crate::sesame::types::{SpiffeUri, WorkloadType};

/// The longest one signing may take. A follower's request carries its own
/// ten-second limit; this bounds the leader's path as well.
const WORKLOAD_SIGNING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A signing whose task is still running.
pub(super) struct IdentitySigning {
    instance_id: InstanceId,
    /// Deploy workers waiting for the identity to be stored (or to fail).
    waiters: Vec<oneshot::Sender<()>>,
}

/// Signings in flight, by the task that runs them.
pub(super) type IdentitySignings = HashMap<tokio::task::Id, IdentitySigning>;

/// What a signing task hands back to the loop.
pub(super) struct SignedIdentity {
    spiffe_uri: SpiffeUri,
    private_key_der: Vec<u8>,
    result: Result<SignedWorkload, String>,
}

/// One finished signing, as `JoinSet::join_next_with_id` yields it.
pub(super) type IdentitySigningOutcome =
    Result<(tokio::task::Id, SignedIdentity), tokio::task::JoinError>;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Start provisioning `instance_id`'s workload identity.
    ///
    /// Returns at once. `reply`, if given, is answered once the identity is
    /// stored or the attempt has failed; a failure is retried by the rotation
    /// tick. A request for an instance whose signing is already running
    /// joins it. Without a council (standalone mode) there is nothing to do.
    pub(super) fn begin_identity_provision(
        &mut self,
        app_name: &str,
        namespace: &str,
        instance_id: &InstanceId,
        is_job: bool,
        reply: Option<oneshot::Sender<()>>,
    ) {
        let Some(council) = self
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.council.clone())
        else {
            answer(reply);
            return;
        };
        if let Some(signing) = self
            .identity_signings
            .values_mut()
            .find(|signing| &signing.instance_id == instance_id)
        {
            signing.waiters.extend(reply);
            return;
        }

        let workload_type = if is_job {
            WorkloadType::Job
        } else {
            WorkloadType::App
        };
        let spiffe_uri =
            super::workload_spiffe_uri(&self.trust_domain, namespace, app_name, workload_type);
        let (csr_der, private_key_der) =
            match crate::sesame::identity::create_workload_csr(&spiffe_uri) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("bun: identity for {instance_id}: CSR generation failed: {error}");
                    answer(reply);
                    return;
                }
            };

        let request = SigningRequest {
            council,
            client: self.workload_csr_client.clone(),
            trust_domain: self.trust_domain.clone(),
            spiffe_uri: spiffe_uri.clone(),
            instance_id: instance_id.0.clone(),
            workload_type,
            csr_der,
        };
        let task = self
            .identity_signing_tasks
            .spawn(async move {
                let result = tokio::time::timeout(WORKLOAD_SIGNING_TIMEOUT, request.sign())
                    .await
                    .unwrap_or_else(|_| Err("workload signing timed out".to_string()));
                SignedIdentity {
                    spiffe_uri,
                    private_key_der,
                    result,
                }
            })
            .id();
        self.identity_signings.insert(
            task,
            IdentitySigning {
                instance_id: instance_id.clone(),
                waiters: reply.into_iter().collect(),
            },
        );
    }

    /// Store a finished signing's identity and answer everyone waiting on it.
    pub(super) fn finish_identity_provision(&mut self, outcome: IdentitySigningOutcome) {
        let (task, signed) = match outcome {
            Ok((task, signed)) => (task, Some(signed)),
            Err(error) => {
                eprintln!("bun: workload identity signing task failed: {error}");
                (error.id(), None)
            }
        };
        let Some(signing) = self.identity_signings.remove(&task) else {
            return;
        };
        if let Some(signed) = signed {
            self.store_signed_identity(&signing.instance_id, signed);
        }
        for waiter in signing.waiters {
            let _ = waiter.send(());
        }
    }

    /// Stop waiting on signings when the agent shuts down. Their waiters are
    /// dropped unanswered, which a deploy worker reads as "carry on".
    pub(super) fn abandon_identity_signings(&mut self) {
        self.identity_signing_tasks.abort_all();
        self.identity_signings.clear();
    }

    /// Write a signed identity to the instance's mount and record it.
    fn store_signed_identity(&mut self, instance_id: &InstanceId, signed: SignedIdentity) {
        let signed_workload = match signed.result {
            Ok(signed_workload) => signed_workload,
            Err(error) => {
                eprintln!("bun: identity for {instance_id}: CSR signing failed: {error}");
                return;
            }
        };
        // Retirement removed the instance's identity directory while its CSR
        // was out. Writing now would recreate it with no owner to remove it.
        if self.supervisor.get_instance(instance_id).is_none() {
            return;
        }
        let identity = crate::sesame::identity::build_identity_bundle(
            signed.spiffe_uri,
            signed_workload.cert_der,
            signed.private_key_der,
            &signed_workload.ca_bundle_der,
            signed_workload.jwt_token.unwrap_or_default(),
        );
        // Write to the instance's own identity mount (PKI7). The directory was
        // prepared before the container was created; a rotation for an adopted
        // instance may find it missing, so prepare it (idempotently) here too.
        let identity_dir = self.instance_identity_dir(instance_id);
        if let Err(error) = crate::sesame::identity::prepare_identity_dir(&identity_dir) {
            eprintln!("bun: identity for {instance_id}: failed to prepare directory: {error}");
            return;
        }
        if let Err(error) = crate::sesame::identity::write_identity_files(
            &identity,
            &identity_dir,
            Self::workload_identity_owner(&identity_dir),
        ) {
            eprintln!("bun: identity for {instance_id}: failed to write files: {error}");
            return;
        }
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.identity = Some(identity);
            instance.identity_mount = Some(identity_dir);
        }
    }
}

/// Everything a signing task needs, owned so the task can outlive the call.
struct SigningRequest {
    council: Arc<CouncilNode>,
    client: Option<WorkloadCsrClient>,
    trust_domain: String,
    spiffe_uri: SpiffeUri,
    instance_id: String,
    workload_type: WorkloadType,
    csr_der: Vec<u8>,
}

impl SigningRequest {
    /// Have the leader sign the CSR: locally when this node leads, over the
    /// leader transport otherwise.
    async fn sign(self) -> Result<SignedWorkload, String> {
        // Only the leader can sign. A follower used to call its own council,
        // fail, and start the container with no identity at all.
        if self.council.is_leader().await {
            return self
                .council
                .sign_workload_csr(
                    &self.csr_der,
                    &self.spiffe_uri,
                    crate::sesame::identity::CertUsage::Mtls,
                    &self.trust_domain,
                    "local",
                    &self.instance_id,
                )
                .await
                .map(|signed| SignedWorkload {
                    cert_der: signed.cert_der,
                    workload_ca_cert_der: signed.workload_ca_cert_der,
                    root_ca_cert_der: signed.root_ca_cert_der,
                    ca_bundle_der: signed.ca_bundle_der,
                    jwt_token: signed.jwt_token,
                })
                .map_err(|error| error.to_string());
        }
        match &self.client {
            Some(client) => client
                .sign(&self.instance_id, self.workload_type, &self.csr_der)
                .await
                .map_err(|error| error.to_string()),
            None => Err("no leader transport for workload signing".to_string()),
        }
    }
}

fn answer(reply: Option<oneshot::Sender<()>>) {
    if let Some(reply) = reply {
        let _ = reply.send(());
    }
}

#[cfg(test)]
impl SignedIdentity {
    /// A finished signing carrying `result`, for tests of the loop's side.
    pub(super) fn for_test(
        spiffe_uri: SpiffeUri,
        private_key_der: Vec<u8>,
        result: Result<SignedWorkload, String>,
    ) -> Self {
        Self {
            spiffe_uri,
            private_key_der,
            result,
        }
    }
}

#[cfg(test)]
impl IdentitySigning {
    /// A signing in flight for `instance_id` with nobody waiting on it.
    pub(super) fn for_test(instance_id: InstanceId) -> Self {
        Self {
            instance_id,
            waiters: Vec::new(),
        }
    }

    /// How many callers are waiting on this signing.
    pub(super) fn waiter_count(&self) -> usize {
        self.waiters.len()
    }
}

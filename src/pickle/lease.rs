//! Repository ownership, writer fencing and confirmed lease retirement.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::api::PickleState;
use super::authority::{
    RegistryMutation, RegistryQuery, RegistryQueryResponse, RegistryRetirement,
};
use super::types::PickleError;

/// Blocking transactions retain the same reader after their request is cancelled.
pub(crate) type RepositoryReadGuard = Arc<OwnedRwLockReadGuard<()>>;

/// Serialise retirement against all writers of one repository on this node.
#[derive(Clone, Default)]
pub struct RepositoryWriters {
    inner: Arc<Mutex<BTreeMap<String, Arc<RwLock<()>>>>>,
}

impl RepositoryWriters {
    async fn gate(&self, repository: &str) -> Result<Arc<RwLock<()>>, PickleError> {
        let mut entries = self.inner.lock().await;
        // Pending callers own their Arc before awaiting a guard. Only the map's
        // sole reference can be forgotten without creating a second lock.
        entries.retain(|_, gate| Arc::strong_count(gate) > 1);
        if let Some(gate) = entries.get(repository) {
            return Ok(gate.clone());
        }
        let maximum = crate::testkit::lease::MAX_ACTIVE_TEST_LEASES
            * crate::testkit::lease::MAX_LEASED_RESOURCES;
        if entries.len() >= maximum {
            return Err(unavailable("too many active repository writers"));
        }
        let gate = Arc::new(RwLock::new(()));
        entries.insert(repository.into(), gate.clone());
        Ok(gate)
    }

    async fn enter(&self, repository: &str) -> Result<RepositoryReadGuard, PickleError> {
        Ok(Arc::new(self.gate(repository).await?.read_owned().await))
    }

    async fn fence(&self, repository: &str) -> Result<OwnedRwLockWriteGuard<()>, PickleError> {
        Ok(self.gate(repository).await?.write_owned().await)
    }
}

/// Evidence retained from admission until the last transaction finishes.
#[derive(Clone, Default)]
pub(crate) struct RegistryWriteAccess {
    pub lease_id: Option<String>,
    pub guard: Option<RepositoryReadGuard>,
}

/// The reserved prefix is a namespace, never a rewritten repository name.
pub(crate) fn is_test_repository(repository: &str) -> bool {
    repository
        .split_once('/')
        .is_some_and(|(namespace, _)| namespace.starts_with("rbtest-"))
}

fn unavailable(message: impl Into<String>) -> PickleError {
    PickleError::ReplicationFailed(message.into())
}

impl PickleState {
    pub(crate) async fn registry_query(
        &self,
        query: RegistryQuery,
    ) -> Result<RegistryQueryResponse, PickleError> {
        if let Some(forwarder) = &self.forwarder {
            return forwarder
                .query(self.council.as_ref(), self.node_raft_id, query)
                .await;
        }
        if let Some(council) = &self.council {
            let security = council
                .security_state_linearizable()
                .await
                .map_err(|e| unavailable(e.to_string()))?;
            if security
                .crl
                .retired_nodes
                .keys()
                .any(|name| crate::cluster::identity::raft_id_from_name(name) == self.node_raft_id)
            {
                return Err(PickleError::LeaseDenied(
                    "registry node identity is retired".into(),
                ));
            }
            return Ok(query.answer(&council.desired_state().await, self.node_raft_id));
        }
        if matches!(query, RegistryQuery::GcGeneration) {
            return Ok(RegistryQueryResponse::GcGeneration(0));
        }
        let state = match &query {
            RegistryQuery::Images
            | RegistryQuery::Repository { .. }
            | RegistryQuery::Usage { .. } => crate::council::DesiredState {
                manifest_catalog: self.catalog.read().await.clone(),
                ..Default::default()
            },
            _ => crate::council::DesiredState {
                test_leases: self
                    .test_leases
                    .snapshot()
                    .await
                    .map_err(|e| unavailable(e.to_string()))?,
                ..Default::default()
            },
        };
        Ok(query.answer(&state, self.node_raft_id))
    }

    /// Reserve this writer in durable authority and local storage before bytes.
    pub(crate) async fn admit_repository_write(
        &self,
        repository: &str,
        lease_id: Option<&str>,
        owner_id: Option<&str>,
        internal: bool,
    ) -> Result<RegistryWriteAccess, PickleError> {
        if !is_test_repository(repository) {
            if lease_id.is_some() {
                return Err(PickleError::LeaseDenied(
                    "leased repository must belong to its lease namespace".into(),
                ));
            }
            return Ok(RegistryWriteAccess::default());
        }
        let lease_id = match (lease_id, internal) {
            (Some(id), false) if owner_id.is_some() => id.to_owned(),
            (None, true) => match self
                .registry_query(RegistryQuery::Lease {
                    repository: repository.into(),
                })
                .await?
            {
                RegistryQueryResponse::Lease(Some(id)) => id,
                _ => {
                    return Err(PickleError::LeaseDenied(
                        "repository has no active lease".into(),
                    ));
                }
            },
            _ => {
                return Err(PickleError::LeaseDenied(
                    "test repository requires its exact authenticated lease owner".into(),
                ));
            }
        };
        let guard = self.repository_writers.enter(repository).await?;
        let now = crate::testkit::lease::now_unix_millis();
        let owner = if internal { None } else { owner_id };
        let response = self
            .propose(RegistryMutation::ClaimWriter {
                lease_id: lease_id.clone(),
                repository: repository.into(),
                node_id: self.node_raft_id,
                owner_id: owner.map(str::to_owned),
            })
            .await?;
        if let Some(response) = response {
            require_acceptance(response)?;
        } else {
            self.test_leases
                .register_registry_writer(&lease_id, repository, self.node_raft_id, owner, now)
                .await
                .map_err(|e| PickleError::LeaseDenied(e.to_string()))?;
        }

        let mut catalog = self.catalog.clone().write_owned().await;
        let repository = repository.to_owned();
        let lease = lease_id.clone();
        let persist = self.persist_path.clone();
        let transaction_guard = guard.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = transaction_guard;
            let mut next = catalog.clone();
            next.claim_repository(&repository, &lease)?;
            if let Some(path) = persist {
                next.persist_to(&path)?;
            }
            *catalog = next;
            Ok::<_, PickleError>(())
        })
        .await
        .map_err(|e| unavailable(format!("registry ownership task failed: {e}")))??;
        Ok(RegistryWriteAccess {
            lease_id: Some(lease_id),
            guard: Some(guard),
        })
    }

    /// Create and register a temporary file as one owned asynchronous operation.
    /// Cancellation of its caller cannot strand a file outside session ownership.
    pub(crate) async fn initiate_owned_upload(
        &self,
        repository: &str,
        principal: Option<&str>,
        access: &RegistryWriteAccess,
    ) -> Result<String, PickleError> {
        let state = self.clone();
        let repository = repository.to_owned();
        let principal = principal.map(str::to_owned);
        let guard = access.guard.clone();
        tokio::spawn(async move {
            let _writer = guard;
            let id = state.store.initiate_upload().await?;
            state
                .sessions
                .register(
                    &id,
                    &repository,
                    principal.as_deref(),
                    std::time::SystemTime::now(),
                )
                .await;
            Ok::<_, PickleError>(id)
        })
        .await
        .map_err(|e| unavailable(format!("upload admission task failed: {e}")))?
    }

    /// Remove local payload only for an exact, authoritative retirement receipt.
    pub(crate) async fn retire_registry_repository(
        &self,
        receipt: &RegistryRetirement,
    ) -> Result<(), PickleError> {
        let guard = self.repository_writers.fence(&receipt.repository).await?;
        // Check the generation before touching upload sessions. Writers cannot
        // change it while the repository's exclusive gate is held.
        self.catalog
            .read()
            .await
            .check_repository_owner(&receipt.repository, &receipt.lease_id)?;
        self.sessions
            .cleanup_repository(&self.store, &receipt.repository)
            .await?;
        let mut catalog = self.catalog.clone().write_owned().await;
        let repository = receipt.repository.clone();
        let lease_id = receipt.lease_id.clone();
        let persist = self.persist_path.clone();
        let (guard, result) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let mut next = catalog.clone();
                next.retire_leased_repository(&repository, &lease_id)?;
                if let Some(path) = persist {
                    next.persist_to(&path)?;
                }
                *catalog = next;
                Ok::<_, PickleError>(())
            })();
            (guard, result)
        })
        .await
        .map_err(|e| unavailable(format!("registry retirement task failed: {e}")))?;
        result?;
        let response = self
            .propose(RegistryMutation::WriterRetired {
                lease_id: receipt.lease_id.clone(),
                repository: receipt.repository.clone(),
                node_id: self.node_raft_id,
            })
            .await?;
        if let Some(response) = response {
            require_acceptance(response)?;
        } else {
            self.test_leases
                .record_registry_retirement(
                    &receipt.lease_id,
                    &receipt.repository,
                    self.node_raft_id,
                )
                .await
                .map_err(|e| unavailable(e.to_string()))?;
        }
        drop(guard);
        Ok(())
    }

    /// Visit every owned ready repository; an unavailable writer cannot starve others.
    pub async fn reap_registry_leases_once(&self) -> Result<(), PickleError> {
        let RegistryQueryResponse::Retirements(receipts) =
            self.registry_query(RegistryQuery::Retirements).await?
        else {
            return Err(unavailable("invalid registry retirement response"));
        };
        let mut failures = 0;
        for receipt in receipts {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.retire_registry_repository(&receipt),
            )
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failures += 1;
                    eprintln!(
                        "pickle: repository {} retirement pending: {error}",
                        receipt.repository
                    );
                }
                Err(_) => {
                    failures += 1;
                    eprintln!(
                        "pickle: repository {} retirement timed out; ownership retained",
                        receipt.repository
                    );
                }
            }
        }
        if failures > 0 {
            return Err(unavailable(format!(
                "{failures} repository retirements remain pending"
            )));
        }
        Ok(())
    }

    /// Reclaim only this node's obligations until shutdown, retaining uncertain work.
    pub async fn run_registry_lease_reaper(self, shutdown: tokio_util::sync::CancellationToken) {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = shutdown.cancelled() => return, _ = ticker.tick() => {} }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                result = self.reap_registry_leases_once() => if let Err(error) = result { eprintln!("pickle: registry cleanup pending: {error}"); }
            }
        }
    }
}

pub(super) fn require_acceptance(
    response: crate::council::CouncilResponse,
) -> Result<(), PickleError> {
    match response {
        crate::council::CouncilResponse::Ok | crate::council::CouncilResponse::Applied { .. } => {
            Ok(())
        }
        crate::council::CouncilResponse::RegistryPublicationStale => Err(unavailable(
            "registry blobs must be verified again after garbage collection; retry",
        )),
        crate::council::CouncilResponse::Refused { reason } => {
            Err(PickleError::LeaseDenied(reason))
        }
        other => Err(unavailable(format!(
            "registry lease proposal refused: {other:?}"
        ))),
    }
}

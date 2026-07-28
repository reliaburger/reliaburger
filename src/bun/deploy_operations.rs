//! Observable state for the deploy path that actually runs in Bun.
//!
//! `meat::DeployState` describes the library-side rolling state machine. The
//! agent's real apply path can contain several apps and jobs and runs on a
//! spawned worker, so it needs a separate operation record for APIs and future
//! diagnostics. This store is deliberately bounded and contains no workload
//! mutation; the agent loop remains the single owner of runtime state.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::Config;

/// Maximum terminal operations retained for diagnostics.
pub const DEPLOY_OPERATION_HISTORY_LIMIT: usize = 50;
/// Maximum concurrent deploy workers accepted by one Bun process.
pub const ACTIVE_DEPLOY_OPERATION_LIMIT: usize = 64;

static LAST_OPERATION_ID: AtomicU64 = AtomicU64::new(0);

/// Stable identifier carried from acceptance through terminal history.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeployOperationId(String);

impl DeployOperationId {
    /// Allocate a process-independent, time-based monotonic identifier.
    pub fn next() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        let mut observed = LAST_OPERATION_ID.load(Ordering::Relaxed);
        loop {
            let candidate = now.max(observed.saturating_add(1));
            match LAST_OPERATION_ID.compare_exchange_weak(
                observed,
                candidate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Self(format!("deploy-{candidate}")),
                Err(current) => observed = current,
            }
        }
    }

    /// Borrow the wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DeployOperationId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for DeployOperationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for DeployOperationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Kind of workload named by an apply operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployTargetKind {
    App,
    Job,
}

/// One app or job touched by an apply operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeployTarget {
    pub kind: DeployTargetKind,
    pub name: String,
    pub namespace: String,
}

/// Coarse phase emitted by the real apply worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployOperationPhase {
    Accepted,
    DeployingApps,
    DeployingJobs,
    RebuildingRoutes,
    Finished,
}

/// Terminal result. `Unknown` means the worker disappeared without a terminal
/// event, rather than silently turning missing evidence into success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployOperationOutcome {
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

/// API-facing record for one real apply operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployOperation {
    pub id: DeployOperationId,
    pub phase: DeployOperationPhase,
    pub outcome: Option<DeployOperationOutcome>,
    pub started_at: u64,
    pub phase_changed_at: u64,
    pub finished_at: Option<u64>,
    pub targets: Vec<DeployTarget>,
    pub current_target: Option<DeployTarget>,
    pub message: String,
}

/// Complete deploy-operation view. History is newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployOperationSnapshot {
    pub active_deploys: Vec<DeployOperation>,
    pub history: Vec<DeployOperation>,
}

/// Backwards-compatible payload for `/v1/deploys/active`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveDeployOperations {
    pub active_deploys: Vec<DeployOperation>,
}

/// Refusal to create another active operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartDeployOperationError {
    #[error("too many active deploy operations ({limit})")]
    ActiveLimit { limit: usize },
    #[error("target {target:?} is already being changed by {operation_id}")]
    TargetBusy {
        target: DeployTarget,
        operation_id: DeployOperationId,
    },
}

/// In-memory active and bounded terminal records.
#[derive(Debug)]
pub struct DeployOperationStore {
    active: BTreeMap<DeployOperationId, DeployOperation>,
    history: VecDeque<DeployOperation>,
    history_limit: usize,
}

impl DeployOperationStore {
    pub fn new(history_limit: usize) -> Self {
        Self {
            active: BTreeMap::new(),
            history: VecDeque::new(),
            history_limit,
        }
    }

    pub fn start(
        &mut self,
        id: DeployOperationId,
        mut targets: Vec<DeployTarget>,
        now: u64,
    ) -> Result<(), StartDeployOperationError> {
        if self.active.len() >= ACTIVE_DEPLOY_OPERATION_LIMIT {
            return Err(StartDeployOperationError::ActiveLimit {
                limit: ACTIVE_DEPLOY_OPERATION_LIMIT,
            });
        }
        targets.sort();
        targets.dedup();
        for target in &targets {
            if let Some((operation_id, _)) = self.active.iter().find(|(_, operation)| {
                operation.targets.iter().any(|active| {
                    active.name == target.name && active.namespace == target.namespace
                })
            }) {
                return Err(StartDeployOperationError::TargetBusy {
                    target: target.clone(),
                    operation_id: operation_id.clone(),
                });
            }
        }
        self.active.insert(
            id.clone(),
            DeployOperation {
                id,
                phase: DeployOperationPhase::Accepted,
                outcome: None,
                started_at: now,
                phase_changed_at: now,
                finished_at: None,
                targets,
                current_target: None,
                message: "deploy accepted".to_string(),
            },
        );
        Ok(())
    }

    pub fn advance(
        &mut self,
        id: &DeployOperationId,
        phase: DeployOperationPhase,
        current_target: Option<DeployTarget>,
        message: String,
        now: u64,
    ) -> bool {
        let Some(operation) = self.active.get_mut(id) else {
            return false;
        };
        operation.phase = phase;
        operation.phase_changed_at = now;
        operation.current_target = current_target;
        operation.message = message;
        true
    }

    pub fn finish(
        &mut self,
        id: &DeployOperationId,
        outcome: DeployOperationOutcome,
        message: String,
        now: u64,
    ) -> bool {
        let Some(mut operation) = self.active.remove(id) else {
            return false;
        };
        operation.phase = DeployOperationPhase::Finished;
        operation.outcome = Some(outcome);
        operation.phase_changed_at = now;
        operation.finished_at = Some(now);
        operation.message = message;
        if self.history_limit > 0 {
            self.history.push_front(operation);
            self.history.truncate(self.history_limit);
        }
        true
    }

    pub fn snapshot(&self) -> DeployOperationSnapshot {
        let mut active_deploys: Vec<_> = self.active.values().cloned().collect();
        active_deploys.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        DeployOperationSnapshot {
            active_deploys,
            history: self.history.iter().cloned().collect(),
        }
    }
}

/// Shared operation registry used by the agent, workers and API query command.
#[derive(Debug, Clone)]
pub struct DeployOperationTracker {
    store: Arc<RwLock<DeployOperationStore>>,
}

impl Default for DeployOperationTracker {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(DeployOperationStore::new(
                DEPLOY_OPERATION_HISTORY_LIMIT,
            ))),
        }
    }
}

impl DeployOperationTracker {
    pub async fn start(
        &self,
        config: &Config,
    ) -> Result<DeployOperationHandle, StartDeployOperationError> {
        let id = DeployOperationId::next();
        self.store
            .write()
            .await
            .start(id.clone(), targets(config), unix_seconds())?;
        Ok(DeployOperationHandle {
            id,
            tracker: self.clone(),
        })
    }

    pub async fn snapshot(&self) -> DeployOperationSnapshot {
        self.store.read().await.snapshot()
    }
}

/// Capability held by exactly the tasks which may update one operation.
#[derive(Debug, Clone)]
pub struct DeployOperationHandle {
    id: DeployOperationId,
    tracker: DeployOperationTracker,
}

impl DeployOperationHandle {
    pub fn id(&self) -> &DeployOperationId {
        &self.id
    }

    pub async fn advance(
        &self,
        phase: DeployOperationPhase,
        current_target: Option<DeployTarget>,
        message: impl Into<String>,
    ) -> bool {
        self.tracker.store.write().await.advance(
            &self.id,
            phase,
            current_target,
            message.into(),
            unix_seconds(),
        )
    }

    pub async fn finish(
        &self,
        outcome: DeployOperationOutcome,
        message: impl Into<String>,
    ) -> bool {
        self.tracker
            .store
            .write()
            .await
            .finish(&self.id, outcome, message.into(), unix_seconds())
    }
}

fn targets(config: &Config) -> Vec<DeployTarget> {
    let apps = config.app.iter().map(|(name, spec)| DeployTarget {
        kind: DeployTargetKind::App,
        name: name.clone(),
        namespace: spec
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    });
    let jobs = config.job.iter().map(|(name, spec)| DeployTarget {
        kind: DeployTargetKind::Job,
        name: name.clone(),
        namespace: spec
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
    });
    let mut targets: Vec<_> = apps.chain(jobs).collect();
    targets.sort();
    targets
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str) -> DeployTarget {
        DeployTarget {
            kind: DeployTargetKind::App,
            name: name.to_string(),
            namespace: "default".to_string(),
        }
    }

    #[test]
    fn operation_ids_are_unique_and_ordered() {
        let first = DeployOperationId::next();
        let second = DeployOperationId::next();
        assert_ne!(first, second);
        assert!(first.as_str().starts_with("deploy-"));
        assert!(first < second);
    }

    #[test]
    fn start_exposes_stable_targets_and_rejects_overlap() {
        let mut store = DeployOperationStore::new(50);
        let id = DeployOperationId::from("deploy-100");
        store
            .start(id.clone(), vec![target("worker"), target("api")], 10)
            .unwrap();

        let snapshot = store.snapshot();
        assert_eq!(snapshot.active_deploys.len(), 1);
        assert_eq!(snapshot.active_deploys[0].id, id);
        assert_eq!(snapshot.active_deploys[0].started_at, 10);
        assert_eq!(
            snapshot.active_deploys[0].phase,
            DeployOperationPhase::Accepted
        );
        assert_eq!(
            snapshot.active_deploys[0]
                .targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "worker"]
        );

        let error = store
            .start(
                DeployOperationId::from("deploy-101"),
                vec![target("api")],
                11,
            )
            .unwrap_err();
        assert!(error.to_string().contains("deploy-100"));
    }

    #[test]
    fn phase_and_terminal_outcome_are_queryable() {
        let mut store = DeployOperationStore::new(50);
        let id = DeployOperationId::from("deploy-200");
        store.start(id.clone(), vec![target("api")], 20).unwrap();
        assert!(store.advance(
            &id,
            DeployOperationPhase::DeployingApps,
            Some(target("api")),
            "starting api".to_string(),
            21,
        ));
        assert!(store.finish(
            &id,
            DeployOperationOutcome::Failed,
            "health check timed out".to_string(),
            25,
        ));

        let snapshot = store.snapshot();
        assert!(snapshot.active_deploys.is_empty());
        assert_eq!(snapshot.history.len(), 1);
        let finished = &snapshot.history[0];
        assert_eq!(finished.id, id);
        assert_eq!(finished.phase, DeployOperationPhase::Finished);
        assert_eq!(finished.outcome, Some(DeployOperationOutcome::Failed));
        assert_eq!(finished.finished_at, Some(25));
        assert_eq!(finished.phase_changed_at, 25);
        assert_eq!(finished.current_target, Some(target("api")));
        assert_eq!(finished.message, "health check timed out");
    }

    #[test]
    fn terminal_history_is_newest_first_and_bounded() {
        let mut store = DeployOperationStore::new(2);
        for number in 1..=3 {
            let id = DeployOperationId::from(format!("deploy-{number}"));
            store.start(id.clone(), Vec::new(), number).unwrap();
            store.finish(
                &id,
                DeployOperationOutcome::Completed,
                "done".to_string(),
                number + 10,
            );
        }

        let history = store.snapshot().history;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id.as_str(), "deploy-3");
        assert_eq!(history[1].id.as_str(), "deploy-2");
    }
}

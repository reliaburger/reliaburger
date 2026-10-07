//! Every task array the cluster knows about, as the Raft state machine
//! holds them.
//!
//! [`TaskArrays`] is one field of the replicated `DesiredState`. It changes
//! only through [`TaskArrayWrite`]s, which the leader proposes and every
//! replica applies in log order with [`TaskArrays::apply`]. Standalone nodes
//! (no council) apply the same writes to an in-memory copy, so there's one
//! set of rules either way.
//!
//! The writes are few by design: one `Register` per submission, at most one
//! `Sync` per array per leader tick (carrying every finished chunk and every
//! new grant since the last one), and a `Cancel` or `Requeue` when those
//! happen. A million tasks cost a few hundred entries, not millions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::index_set::IndexRangeSet;
use super::task_array::{TaskArraySpec, TaskArraySpecError, validate_template};
use super::task_array_state::{ChunkResult, TaskArrayState};
use super::types::NodeId;
use crate::config::job::JobSpec;

/// How long a finished array stays readable (status, results, logs)
/// before a later registration prunes it.
pub const TERMINAL_RETENTION_SECS: u64 = 3600;

/// Most finished arrays kept, newest first. Each can hold up to about
/// 160 KiB of failed-index ranges, so this bounds the snapshot too.
pub const MAX_TERMINAL_ARRAYS: usize = 20;

/// Most arrays running at once. The leader syncs every node for every
/// running array once a second, so this bounds that work.
pub const MAX_ACTIVE_ARRAYS: usize = 64;

/// One submitted array: what to run, and how far it has got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArrayRecord {
    /// The name given at submission (`relish run --batch NAME`).
    pub name: String,
    /// Namespace the array belongs to.
    pub namespace: String,
    /// The job every task runs, with `{index}` placeholders.
    pub template: JobSpec,
    /// Chunks, grants and counts.
    pub state: TaskArrayState,
    /// Timestamp of the accepted terminal transition, supplied by the leader.
    pub terminal_at_epoch_secs: Option<u64>,
}

/// One homogeneous resource profile in a mixed submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestCohort {
    pub name: String,
    #[serde(flatten)]
    pub spec: TaskArraySpec,
    pub template: JobSpec,
}
/// Durable mapping from a manifest's profile names to stable array identities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskManifest {
    pub name: String,
    pub namespace: String,
    pub cohorts: Vec<(String, u64)>,
    /// Finite named singleton group; authorise every child rather than a synthetic parent.
    pub common_jobs: bool,
    /// Idempotent finite-group request identity and complete input digest.
    pub request: Option<(String, String)>,
    pub submitted_at_epoch_secs: u64,
}

/// A change to the set of task arrays. Carried by one Raft entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskArrayWrite {
    /// Atomically admit a bounded heterogeneous group of ordinary singletons.
    RegisterJobs {
        request_id: String,
        jobs: Vec<(String, JobSpec)>,
        submitted_at_epoch_secs: u64,
    },
    /// Disable future occurrences and cancel all current runs in one transaction.
    StopDefinition {
        name: String,
        namespace: String,
        forget: bool,
        now_epoch_secs: u64,
    },
    /// Persist deployment ownership and admit only its prerequisite runs.
    DeployBegin {
        operation_id: String,
        config: Box<crate::config::Config>,
        now_epoch_secs: u64,
    },
    /// App publication succeeded; admit ordinary jobs and register schedules.
    DeployCommitted {
        operation_id: String,
        now_epoch_secs: u64,
    },
    /// Cancel the intent before draining any running tasks.
    DeployCancel {
        operation_id: String,
        now_epoch_secs: u64,
    },
    /// Positively settled operations release workload ownership.
    DeployRelease { operation_id: String },
    /// Definition/run transaction using the same indexed execution state.
    Job(Box<super::job::JobWrite>),
    /// Persist the latest observed UTC minute independently of matching schedules.
    CronObserve { minute: i64 },
    /// Retain a conservative run's ownership when its outcome cannot be established.
    Unknown { batch_id: u64, node: NodeId },
    /// An operator acknowledged repeating an unknown owner's side effects.
    Replay {
        batch_id: u64,
        node: NodeId,
        grant_digest: String,
        now_epoch_secs: u64,
    },
    /// Atomically register every profile before any work is dispatched.
    RegisterManifest {
        name: String,
        namespace: String,
        cohorts: Vec<ManifestCohort>,
        submitted_at_epoch_secs: u64,
    },
    /// Cancel every profile through one replicated operation.
    CancelManifest { batch_id: u64, now_epoch_secs: u64 },
    /// Submit a new array. The id comes from the cluster's batch counter,
    /// so array and ordinary batch ids never collide.
    Register {
        name: String,
        namespace: String,
        template: Box<JobSpec>,
        spec: TaskArraySpec,
        /// The submitter's clock; `apply` never reads its own.
        submitted_at_epoch_secs: u64,
    },
    /// Record finished chunks, then hand out new ones.
    Sync {
        now_epoch_secs: u64,
        batch_id: u64,
        /// Chunks nodes finished, each fenced by its grant attempt.
        results: Vec<(NodeId, ChunkResult)>,
        /// New grants, planned by the leader against the state with
        /// `results` applied.
        grants: Vec<(NodeId, IndexRangeSet)>,
    },
    /// Stop an array: nothing new starts, running chunks drain.
    Cancel { batch_id: u64, now_epoch_secs: u64 },
    /// A node went quiet: take its chunks back at the next attempt.
    Requeue {
        batch_id: u64,
        node: NodeId,
        now_epoch_secs: u64,
    },
}

impl TaskArrayWrite {
    /// IDs required by an atomic registration; progress writes allocate none.
    pub fn registration_ids(&self) -> usize {
        match self {
            Self::Register { .. } | Self::Job(_) => 1,
            Self::RegisterManifest { cohorts, .. } => cohorts.len().saturating_add(1),
            _ => 0,
        }
    }
}

/// What an applied write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskArrayApplied {
    /// A definition was recorded or an occurrence deliberately skipped.
    JobRecorded,
    /// A new array exists under this id.
    Registered { batch_id: u64 },
    /// A sync went through. Refused items are stale (a lost node's late
    /// report, a grant raced by a cancel) and are skipped, not fatal.
    Synced {
        results_applied: u32,
        results_refused: u32,
        grants_applied: u32,
        grants_refused: u32,
    },
    /// The array is stopping (or had already stopped).
    Cancelled,
    /// These chunks went back to the queue (or were written off, if the
    /// array had stopped).
    Requeued { chunks: IndexRangeSet },
}

/// Why a write was refused as a whole.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskArrayStoreError {
    #[error("invalid job transaction: {0}")]
    Job(String),
    #[error("invalid task manifest: {0}")]
    Manifest(String),
    #[error("task array {batch_id} not found")]
    UnknownArray { batch_id: u64 },
    #[error("invalid task array: {0}")]
    Invalid(#[from] TaskArraySpecError),
    #[error("a task array needs a name")]
    EmptyName,
    #[error("{active} task arrays are already running; the limit is {MAX_ACTIVE_ARRAYS}")]
    TooManyActive { active: usize },
}

/// The replicated set of task arrays, keyed by batch id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ArraysWire", into = "ArraysWire")]
pub struct TaskArrays {
    jobs: super::job::JobCatalog,
    cron_observed_minute: Option<i64>,
    deployments: BTreeMap<String, super::job_deploy::DeploymentRecord>,
    arrays: BTreeMap<u64, TaskArrayRecord>,
    manifests: BTreeMap<u64, TaskManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArraysWire {
    jobs: super::job::JobCatalog,
    cron_observed_minute: Option<i64>,
    deployments: BTreeMap<String, super::job_deploy::DeploymentRecord>,
    arrays: BTreeMap<u64, TaskArrayRecord>,
    manifests: BTreeMap<u64, TaskManifest>,
}
impl From<TaskArrays> for ArraysWire {
    fn from(store: TaskArrays) -> Self {
        Self {
            jobs: store.jobs,
            cron_observed_minute: store.cron_observed_minute,
            deployments: store.deployments,
            arrays: store.arrays,
            manifests: store.manifests,
        }
    }
}
impl TryFrom<ArraysWire> for TaskArrays {
    type Error = String;
    fn try_from(wire: ArraysWire) -> Result<Self, String> {
        let store = Self {
            jobs: wire.jobs,
            cron_observed_minute: wire.cron_observed_minute,
            deployments: wire.deployments,
            arrays: wire.arrays,
            manifests: wire.manifests,
        };
        if store.cron_observed_minute.is_some_and(|minute| minute < 0)
            || store.active().count() > MAX_ACTIVE_ARRAYS
            || store.arrays.len() > 2048
            || store.manifests.len() > 84
            || store
                .deployments
                .values()
                .filter(|record| !record.completed)
                .count()
                > 64
            || store
                .deployments
                .values()
                .filter(|record| record.completed)
                .count()
                > 20
        {
            return Err("job state exceeds its capacity or clock bounds".into());
        }
        for (id, record) in &store.arrays {
            if *id == 0
                || !crate::config::valid_workload_label(&record.name)
                || !crate::config::valid_workload_label(&record.namespace)
            {
                return Err("invalid run identity".into());
            }
            validate_template(&record.template).map_err(|error| error.to_string())?;
            record.state.validate_snapshot()?;
            if record.state.status().is_terminal() != record.terminal_at_epoch_secs.is_some() {
                return Err("run has inconsistent terminal receipt".into());
            }
        }
        for (id, run) in store.jobs.runs() {
            if store
                .get(id)
                .is_none_or(|record| record.name != run.name || record.namespace != run.namespace)
            {
                return Err("run provenance has no matching execution state".into());
            }
        }
        let mut children = std::collections::BTreeSet::new();
        for (id, manifest) in &store.manifests {
            if *id == 0
                || store.arrays.contains_key(id)
                || manifest.cohorts.is_empty()
                || manifest.cohorts.len() > if manifest.common_jobs { 64 } else { 16 }
            {
                return Err("invalid manifest identity or profile count".into());
            }
            let mut names = std::collections::BTreeSet::new();
            for (name, child) in &manifest.cohorts {
                if !names.insert(name)
                    || !children.insert(*child)
                    || store.get(*child).is_none()
                    || (manifest.common_jobs && store.jobs.run(*child).is_none())
                {
                    return Err("manifest has a missing or duplicate profile".into());
                }
            }
        }
        let mut owned = std::collections::BTreeSet::new();
        for (id, record) in &store.deployments {
            if id.len() != 32
                || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
                || serde_json::to_vec(&record.config).map_or(true, |bytes| bytes.len() > 128 * 1024)
            {
                return Err("invalid deployment identity or size".into());
            }
            record
                .config
                .validate_intrinsic()
                .map_err(|error| error.to_string())?;
            let expected: std::collections::BTreeSet<_> = record
                .config
                .job
                .iter()
                .filter(|(_, spec)| !spec.run_before.is_empty())
                .map(|(name, _)| name)
                .collect();
            if record
                .hook_runs
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
                != expected
            {
                return Err("deployment has missing or unexpected hooks".into());
            }
            let expected_jobs: std::collections::BTreeSet<_> = record
                .config
                .job
                .iter()
                .filter(|(_, spec)| spec.run_before.is_empty() && spec.schedule.is_none())
                .map(|(name, _)| name)
                .collect();
            if (record.apps_committed
                && record
                    .job_runs
                    .keys()
                    .collect::<std::collections::BTreeSet<_>>()
                    != expected_jobs)
                || (!record.apps_committed && !record.job_runs.is_empty())
            {
                return Err("deployment has inconsistent ordinary run identities".into());
            }
            if record.completed != record.outcome.is_some()
                || record.completed != record.finished_at.is_some()
            {
                return Err("deployment has inconsistent settlement receipt".into());
            }
            if record.completed {
                continue;
            }
            for (name, run_id) in record.hook_runs.iter().chain(&record.job_runs) {
                let spec = record
                    .config
                    .job
                    .get(name)
                    .ok_or("deployment references an unknown job")?;
                let run = store
                    .jobs
                    .run(*run_id)
                    .ok_or("pending deployment lost run provenance")?;
                if run.name != *name
                    || run.namespace != spec.namespace.as_deref().unwrap_or("default")
                    || run.trigger
                        != (super::job::RunTrigger::Hook {
                            operation_id: id.clone(),
                        })
                {
                    return Err("deployment run belongs to a different operation".into());
                }
            }
            if record.apps_committed
                && !record.hook_runs.values().all(|id| {
                    store.get(*id).is_some_and(|run| {
                        run.state.status() == super::task_array_state::TaskArrayStatus::Succeeded
                    })
                })
            {
                return Err("dependent apps have no accepted successful hook proof".into());
            }
            for (name, namespace) in
                record
                    .config
                    .app
                    .iter()
                    .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default")))
                    .chain(
                        record.config.job.iter().map(|(name, spec)| {
                            (name, spec.namespace.as_deref().unwrap_or("default"))
                        }),
                    )
            {
                if record.blocks(name, namespace) && !owned.insert((namespace, name)) {
                    return Err("deployments have conflicting workload ownership".into());
                }
            }
        }
        Ok(store)
    }
}

impl TaskArrays {
    /// Apply one write. `allocate_id` is called only for a registration
    /// that will succeed, so a refused one doesn't burn an id.
    pub fn apply(
        &mut self,
        write: &TaskArrayWrite,
        mut allocate_id: impl FnMut() -> u64,
    ) -> Result<TaskArrayApplied, TaskArrayStoreError> {
        if let TaskArrayWrite::Register {
            name, namespace, ..
        }
        | TaskArrayWrite::RegisterManifest {
            name, namespace, ..
        } = write
            && self.deployment_owner(name, namespace).is_some()
        {
            return Err(TaskArrayStoreError::Job(
                "an unsettled deployment owns this workload".into(),
            ));
        }
        match write {
            TaskArrayWrite::RegisterJobs { .. }
            | TaskArrayWrite::DeployBegin { .. }
            | TaskArrayWrite::DeployCommitted { .. }
            | TaskArrayWrite::DeployCancel { .. }
            | TaskArrayWrite::DeployRelease { .. } => {
                // Preflight the complete transaction, including every definition and ID,
                // before touching the caller's allocation counter or publishing any state.
                self.registration_ids(write)?;
                let mut candidate = self.clone();
                let result = candidate.apply_deployment(write, &mut allocate_id)?;
                *self = candidate;
                Ok(result)
            }
            TaskArrayWrite::StopDefinition {
                name,
                namespace,
                forget,
                now_epoch_secs,
            } => {
                self.jobs
                    .stop(namespace, name, *forget)
                    .map_err(TaskArrayStoreError::Job)?;
                let ids: Vec<_> = self
                    .jobs
                    .runs()
                    .filter(|(_, run)| run.name == *name && run.namespace == *namespace)
                    .map(|(id, _)| id)
                    .collect();
                for id in ids {
                    self.record_mut(id)?.state.cancel();
                    self.mark_terminal(id, *now_epoch_secs)?;
                }
                Ok(TaskArrayApplied::Cancelled)
            }
            TaskArrayWrite::CronObserve { minute } => {
                if *minute < 0 {
                    return Err(TaskArrayStoreError::Job(
                        "cron minute precedes the epoch".into(),
                    ));
                }
                self.cron_observed_minute = self.cron_observed_minute.max(Some(*minute));
                Ok(TaskArrayApplied::JobRecorded)
            }
            TaskArrayWrite::Job(write) => self.apply_job(write, allocate_id),
            TaskArrayWrite::RegisterManifest {
                name,
                namespace,
                cohorts,
                submitted_at_epoch_secs,
            } => {
                if !crate::config::valid_workload_label(name)
                    || !crate::config::valid_workload_label(namespace)
                    || cohorts.is_empty()
                    || cohorts.len() > 16
                {
                    return Err(TaskArrayStoreError::Manifest(
                        "give DNS-label name/namespace and 1–16 resource profiles".into(),
                    ));
                }
                let mut names = std::collections::BTreeSet::new();
                let mut total = 0u64;
                for cohort in cohorts {
                    if !crate::config::valid_workload_label(&cohort.name)
                        || !names.insert(&cohort.name)
                    {
                        return Err(TaskArrayStoreError::Manifest(
                            "profile names must be nonempty and unique".into(),
                        ));
                    }
                    cohort.spec.validate()?;
                    validate_template(&cohort.template)?;
                    total += u64::from(cohort.spec.count);
                }
                if total > u64::from(super::task_array::MAX_TASK_COUNT) {
                    return Err(TaskArrayStoreError::Manifest(
                        "manifest exceeds the task-count bound".into(),
                    ));
                }
                if self.active().count() + cohorts.len() > MAX_ACTIVE_ARRAYS {
                    return Err(TaskArrayStoreError::TooManyActive {
                        active: self.active().count(),
                    });
                }
                self.prune(*submitted_at_epoch_secs);
                let batch_id = allocate_id();
                let mut identities = Vec::new();
                for cohort in cohorts {
                    let id = allocate_id();
                    let state = TaskArrayState::new(cohort.spec.clone(), *submitted_at_epoch_secs)?;
                    self.arrays.insert(
                        id,
                        TaskArrayRecord {
                            name: name.clone(),
                            namespace: namespace.clone(),
                            template: cohort.template.clone(),
                            state,
                            terminal_at_epoch_secs: None,
                        },
                    );
                    identities.push((cohort.name.clone(), id));
                }
                self.manifests.insert(
                    batch_id,
                    TaskManifest {
                        name: name.clone(),
                        namespace: namespace.clone(),
                        cohorts: identities,
                        common_jobs: false,
                        request: None,
                        submitted_at_epoch_secs: *submitted_at_epoch_secs,
                    },
                );
                Ok(TaskArrayApplied::Registered { batch_id })
            }
            TaskArrayWrite::CancelManifest {
                batch_id,
                now_epoch_secs,
            } => {
                let manifest = self
                    .manifests
                    .get(batch_id)
                    .ok_or(TaskArrayStoreError::UnknownArray {
                        batch_id: *batch_id,
                    })?
                    .clone();
                for (_, id) in manifest.cohorts {
                    self.record_mut(id)?.state.cancel();
                    self.mark_terminal(id, *now_epoch_secs)?;
                }
                Ok(TaskArrayApplied::Cancelled)
            }
            TaskArrayWrite::Register {
                name,
                namespace,
                template,
                spec,
                submitted_at_epoch_secs,
            } => {
                if !crate::config::valid_workload_label(name)
                    || !crate::config::valid_workload_label(namespace)
                {
                    return Err(TaskArrayStoreError::EmptyName);
                }
                validate_template(template)?;
                let state = TaskArrayState::new(spec.clone(), *submitted_at_epoch_secs)?;
                self.prune(*submitted_at_epoch_secs);
                let active = self.active().count();
                if active >= MAX_ACTIVE_ARRAYS {
                    return Err(TaskArrayStoreError::TooManyActive { active });
                }
                let batch_id = allocate_id();
                self.arrays.insert(
                    batch_id,
                    TaskArrayRecord {
                        terminal_at_epoch_secs: None,
                        name: name.clone(),
                        namespace: namespace.clone(),
                        template: (**template).clone(),
                        state,
                    },
                );
                Ok(TaskArrayApplied::Registered { batch_id })
            }
            TaskArrayWrite::Sync {
                now_epoch_secs,
                batch_id,
                results,
                grants,
            } => {
                let state = &mut self.record_mut(*batch_id)?.state;
                let mut applied = (0, 0, 0, 0);
                for (node, result) in results {
                    match state.complete(node, result) {
                        Ok(_) => applied.0 += 1,
                        Err(_) => applied.1 += 1,
                    }
                }
                for (node, chunks) in grants {
                    match state.grant(node, chunks) {
                        Ok(()) => applied.2 += 1,
                        Err(_) => applied.3 += 1,
                    }
                }
                self.mark_terminal(*batch_id, *now_epoch_secs)?;
                Ok(TaskArrayApplied::Synced {
                    results_applied: applied.0,
                    results_refused: applied.1,
                    grants_applied: applied.2,
                    grants_refused: applied.3,
                })
            }
            TaskArrayWrite::Cancel {
                batch_id,
                now_epoch_secs,
            } => {
                self.record_mut(*batch_id)?.state.cancel();
                self.mark_terminal(*batch_id, *now_epoch_secs)?;
                Ok(TaskArrayApplied::Cancelled)
            }
            TaskArrayWrite::Unknown { batch_id, node } => {
                if self
                    .get(*batch_id)
                    .ok_or(TaskArrayStoreError::UnknownArray {
                        batch_id: *batch_id,
                    })?
                    .state
                    .held_by(node)
                    .is_none()
                {
                    return Err(TaskArrayStoreError::Job(
                        "unknown observation does not name a held grant".into(),
                    ));
                }
                let run = self
                    .jobs
                    .run_mut(*batch_id)
                    .ok_or_else(|| TaskArrayStoreError::Job("unknown run".into()))?;
                if run.unknown_owners.len() >= 64 && !run.unknown_owners.contains(node) {
                    return Err(TaskArrayStoreError::Job(
                        "unknown owner limit reached".into(),
                    ));
                }
                run.unknown_owners.insert(node.clone());
                Ok(TaskArrayApplied::JobRecorded)
            }
            TaskArrayWrite::Replay {
                batch_id,
                node,
                grant_digest,
                now_epoch_secs,
            } => {
                let run = self
                    .jobs
                    .run(*batch_id)
                    .ok_or_else(|| TaskArrayStoreError::Job("unknown run".into()))?;
                if !run.unknown_owners.contains(node) {
                    return Err(TaskArrayStoreError::Job(
                        "owner has no unresolved execution".into(),
                    ));
                }
                if self.owner_fingerprint(*batch_id, node).as_deref() != Some(grant_digest.as_str())
                {
                    return Err(TaskArrayStoreError::Job(
                        "replay acknowledgement names stale grants".into(),
                    ));
                }
                let chunks = self.record_mut(*batch_id)?.state.requeue_node(node);
                if let Some(run) = self.jobs.run_mut(*batch_id) {
                    run.unknown_owners.remove(node);
                }
                self.mark_terminal(*batch_id, *now_epoch_secs)?;
                Ok(TaskArrayApplied::Requeued { chunks })
            }
            TaskArrayWrite::Requeue {
                batch_id,
                node,
                now_epoch_secs,
            } => {
                if self
                    .jobs
                    .run(*batch_id)
                    .is_some_and(|run| !run.replay_unknown)
                {
                    return Err(TaskArrayStoreError::Job(
                        "unknown outcome requires acknowledged replay".into(),
                    ));
                }
                let chunks = self.record_mut(*batch_id)?.state.requeue_node(node);
                self.mark_terminal(*batch_id, *now_epoch_secs)?;
                Ok(TaskArrayApplied::Requeued { chunks })
            }
        }
    }

    /// IDs needed by this exact transaction; idempotent replays and skipped
    /// occurrences remain admissible even when the shared counter is exhausted.
    pub fn registration_ids(&self, write: &TaskArrayWrite) -> Result<usize, TaskArrayStoreError> {
        if matches!(
            write,
            TaskArrayWrite::RegisterJobs { .. }
                | TaskArrayWrite::DeployBegin { .. }
                | TaskArrayWrite::DeployCommitted { .. }
                | TaskArrayWrite::DeployCancel { .. }
                | TaskArrayWrite::DeployRelease { .. }
        ) {
            let mut candidate = self.clone();
            let mut next = 0u64;
            let mut count = 0;
            candidate.apply_deployment(write, &mut || {
                next += 1;
                while self.arrays.contains_key(&next) || self.manifests.contains_key(&next) {
                    next += 1;
                }
                count += 1;
                next
            })?;
            return Ok(count);
        }
        if let TaskArrayWrite::Job(write) = write
            && let super::job::JobWrite::Fire { minute, .. } = write.as_ref()
            && self
                .cron_observed_minute
                .is_some_and(|observed| observed > *minute)
        {
            return Ok(0);
        }
        if let TaskArrayWrite::Job(write) = write {
            let mut candidate = self.clone();
            let mut next = 0u64;
            let mut count = 0;
            candidate.apply_job(write, || {
                next += 1;
                while self.arrays.contains_key(&next) || self.manifests.contains_key(&next) {
                    next += 1;
                }
                count += 1;
                next
            })?;
            return Ok(count);
        }
        Ok(write.registration_ids())
    }

    /// Counter headroom for all stages known at deployment admission.
    pub fn planned_ids(&self, write: &TaskArrayWrite) -> Result<usize, TaskArrayStoreError> {
        let actual = self.registration_ids(write)?;
        if let TaskArrayWrite::DeployBegin {
            operation_id,
            config,
            ..
        } = write
            && !self.deployments.contains_key(operation_id)
        {
            return Ok(config
                .job
                .values()
                .filter(|spec| spec.schedule.is_none())
                .count()
                .max(actual));
        }
        Ok(actual)
    }

    /// Cluster/standalone schedule clock high-water mark.
    pub fn cron_observed_minute(&self) -> Option<i64> {
        self.cron_observed_minute
    }

    /// Fingerprint an owner's exact held chunk generations for replay acknowledgements.
    pub fn owner_fingerprint(&self, id: u64, node: &NodeId) -> Option<String> {
        use sha2::{Digest, Sha256};
        let record = self.get(id)?;
        let held = record.state.held_by(node)?;
        let generations: Vec<_> = held
            .iter()
            .map(|chunk| {
                (
                    chunk,
                    record.state.attempt_of(super::task_array::ChunkId(chunk)),
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&(id, node, generations)).ok()?;
        Some(hex::encode(Sha256::digest(bytes)))
    }

    /// Reusable definitions and immutable provenance accompanying execution runs.
    pub fn jobs(&self) -> &super::job::JobCatalog {
        &self.jobs
    }

    /// A workload kind cannot reuse an identity while its job history remains visible.
    pub fn job_identity_reserved(&self, namespace: &str, name: &str) -> bool {
        self.jobs.definition(namespace, name).is_some()
            || self
                .jobs
                .runs()
                .any(|(_, run)| run.namespace == namespace && run.name == name)
    }

    fn apply_job(
        &mut self,
        write: &super::job::JobWrite,
        mut allocate_id: impl FnMut() -> u64,
    ) -> Result<TaskArrayApplied, TaskArrayStoreError> {
        if let super::job::JobWrite::Fire { minute, .. } = write
            && self
                .cron_observed_minute
                .is_some_and(|observed| observed > *minute)
        {
            return Ok(TaskArrayApplied::JobRecorded);
        }
        use super::job::JobPlan;
        let active = self.active().map(|(id, _)| id).collect();
        let mut replay = self.jobs.clone();
        if let Ok(super::job::JobPlan::Existing(batch_id)) = replay.prepare(write, &active) {
            return Ok(TaskArrayApplied::Registered { batch_id });
        }
        let now = match write {
            super::job::JobWrite::Put { now_epoch_secs, .. }
            | super::job::JobWrite::Fire { now_epoch_secs, .. } => *now_epoch_secs,
        };
        let mut retained = self.clone();
        retained.prune(now);
        let mut jobs = retained.jobs.clone();
        let plan = jobs
            .prepare(write, &active)
            .map_err(TaskArrayStoreError::Job)?;
        if !matches!(plan, JobPlan::Existing(_)) {
            let (name, namespace, operation) = match write {
                super::job::JobWrite::Put {
                    name,
                    namespace,
                    trigger,
                    ..
                } => (
                    name,
                    namespace,
                    match trigger {
                        Some(super::job::RunTrigger::Hook { operation_id }) => {
                            Some(operation_id.as_str())
                        }
                        _ => None,
                    },
                ),
                super::job::JobWrite::Fire {
                    name, namespace, ..
                } => (name, namespace, None),
            };
            if self
                .deployment_owner(name, namespace)
                .is_some_and(|owner| Some(owner) != operation)
            {
                return Err(TaskArrayStoreError::Job(
                    "a deployment owns this workload".into(),
                ));
            }
        }
        match plan {
            JobPlan::Existing(batch_id) => Ok(TaskArrayApplied::Registered { batch_id }),
            JobPlan::Recorded => {
                self.prune(now);
                self.jobs = jobs;
                Ok(TaskArrayApplied::JobRecorded)
            }
            JobPlan::Register {
                definition,
                run,
                now_epoch_secs,
            } => {
                let state = TaskArrayState::new(definition.tasks, now_epoch_secs)?;
                let active = self.active().count();
                if active >= MAX_ACTIVE_ARRAYS {
                    return Err(TaskArrayStoreError::TooManyActive { active });
                }
                // Every fallible check completes before pruning, allocating or publishing.
                self.prune(now_epoch_secs);
                let batch_id = allocate_id();
                self.arrays.insert(
                    batch_id,
                    TaskArrayRecord {
                        name: run.name.clone(),
                        namespace: run.namespace.clone(),
                        template: definition.template,
                        state,
                        terminal_at_epoch_secs: None,
                    },
                );
                jobs.record_run(batch_id, run);
                let retained = self.arrays.keys().copied().collect();
                jobs.retain_runs(&retained);
                self.jobs = jobs;
                Ok(TaskArrayApplied::Registered { batch_id })
            }
        }
    }

    fn mark_terminal(&mut self, id: u64, now: u64) -> Result<(), TaskArrayStoreError> {
        let record = self.record_mut(id)?;
        if record.state.status().is_terminal() && record.terminal_at_epoch_secs.is_none() {
            record.terminal_at_epoch_secs = Some(now.max(record.state.submitted_at_epoch_secs));
        }
        let state = record.state.clone();
        if let Some(run) = self.jobs.run_mut(id) {
            run.unknown_owners
                .retain(|node| state.held_by(node).is_some());
        }
        Ok(())
    }

    fn record_mut(&mut self, batch_id: u64) -> Result<&mut TaskArrayRecord, TaskArrayStoreError> {
        self.arrays
            .get_mut(&batch_id)
            .ok_or(TaskArrayStoreError::UnknownArray { batch_id })
    }

    /// Drop finished arrays past the retention window, then keep at most
    /// [`MAX_TERMINAL_ARRAYS`] of the rest (newest ids win). `now` comes
    /// from the write, so every replica prunes the same arrays.
    pub(crate) fn prune(&mut self, now_epoch_secs: u64) {
        let pinned: std::collections::BTreeSet<_> = self
            .deployments
            .values()
            .filter(|record| !record.completed)
            .flat_map(|record| record.runs())
            .collect();
        let children: std::collections::BTreeSet<u64> = self
            .manifests
            .values()
            .flat_map(|m| m.cohorts.iter().map(|(_, id)| *id))
            .collect();
        // A parent and all its profiles are one retention unit. Its clock
        // starts when the last profile finishes, not at submission.
        let mut terminal: Vec<(u64, u64, bool)> = self
            .manifests
            .iter()
            .filter_map(|(id, m)| {
                let completed: Option<Vec<u64>> = m
                    .cohorts
                    .iter()
                    .map(|(_, child)| self.arrays.get(child)?.terminal_at_epoch_secs)
                    .collect();
                completed.map(|times| (times.into_iter().max().unwrap_or(0), *id, true))
            })
            .chain(self.arrays.iter().filter_map(|(id, r)| {
                if children.contains(id) || pinned.contains(id) {
                    return None;
                }
                r.terminal_at_epoch_secs.map(|time| (time, *id, false))
            }))
            .collect();
        terminal.sort_unstable();
        let retained = terminal
            .iter()
            .filter(|(time, _, _)| now_epoch_secs.saturating_sub(*time) <= TERMINAL_RETENTION_SECS)
            .count();
        let mut excess = retained.saturating_sub(MAX_TERMINAL_ARRAYS);
        for (time, id, parent) in terminal {
            let expired = now_epoch_secs.saturating_sub(time) > TERMINAL_RETENTION_SECS;
            if !expired && excess == 0 {
                continue;
            }
            if !expired {
                excess -= 1;
            }
            if parent {
                if let Some(manifest) = self.manifests.remove(&id) {
                    for (_, child) in manifest.cohorts {
                        self.arrays.remove(&child);
                    }
                }
            } else {
                self.arrays.remove(&id);
            }
        }
        self.jobs
            .retain_runs(&self.arrays.keys().copied().collect());
    }

    /// Active deployment intent, excluding bounded terminal receipts.
    pub fn deployment(&self, operation_id: &str) -> Option<&super::job_deploy::DeploymentRecord> {
        self.deployments
            .get(operation_id)
            .filter(|record| !record.completed)
    }
    /// Active and recent terminal deployment receipts.
    pub fn deployments(
        &self,
    ) -> impl Iterator<Item = (&str, &super::job_deploy::DeploymentRecord)> {
        self.deployments
            .iter()
            .map(|(id, record)| (id.as_str(), record))
    }
    /// Durable workload ownership, shared by apply, cron and direct run admission.
    pub fn deployment_owner(&self, name: &str, namespace: &str) -> Option<&str> {
        self.deployments()
            .find_map(|(id, record)| record.blocks(name, namespace).then_some(id))
    }
    fn apply_deployment(
        &mut self,
        write: &TaskArrayWrite,
        allocate: &mut impl FnMut() -> u64,
    ) -> Result<TaskArrayApplied, TaskArrayStoreError> {
        use super::{
            job::{JobDefinition, JobWrite, RunTrigger},
            job_deploy::DeploymentRecord,
            task_array_state::TaskArrayStatus,
        };
        let refuse = |reason: &str| TaskArrayStoreError::Job(reason.into());
        match write {
            TaskArrayWrite::RegisterJobs {
                request_id,
                jobs,
                submitted_at_epoch_secs,
            } => {
                use sha2::{Digest, Sha256};
                if request_id.is_empty()
                    || request_id.len() > 128
                    || request_id.chars().any(char::is_control)
                    || jobs.is_empty()
                    || jobs.len() > MAX_ACTIVE_ARRAYS
                {
                    return Err(refuse(
                        "finite batches require a request identity and 1–64 named jobs; use resource profiles for larger submissions",
                    ));
                }
                let mut jobs = jobs.clone();
                jobs.sort_by(|a, b| a.0.cmp(&b.0));
                let mut names = std::collections::BTreeSet::new();
                for (name, spec) in &mut jobs {
                    if !crate::config::valid_workload_label(name)
                        || !names.insert(name.clone())
                        || spec.schedule.is_some()
                        || !spec.run_before.is_empty()
                    {
                        return Err(refuse(
                            "finite batch jobs need distinct DNS-label names and no trigger fields",
                        ));
                    }
                    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
                    if !crate::config::valid_workload_label(&namespace) {
                        return Err(refuse("invalid finite job namespace"));
                    }
                    spec.namespace = Some(namespace);
                    JobDefinition::from_spec(spec.clone())
                        .validate()
                        .map_err(TaskArrayStoreError::Job)?;
                }
                let bytes = serde_json::to_vec(&jobs)
                    .map_err(|error| TaskArrayStoreError::Job(error.to_string()))?;
                let digest = hex::encode(Sha256::digest(bytes));
                if let Some((id, prior)) = self.manifests.iter().find(|(_, record)| {
                    record
                        .request
                        .as_ref()
                        .is_some_and(|(id, _)| id == request_id)
                }) {
                    return if prior
                        .request
                        .as_ref()
                        .is_some_and(|(_, prior)| prior == &digest)
                    {
                        Ok(TaskArrayApplied::Registered { batch_id: *id })
                    } else {
                        Err(refuse("batch request already names different work"))
                    };
                }
                let mut cohorts = Vec::new();
                for (name, spec) in jobs {
                    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
                    let result = self.apply_job(
                        &JobWrite::Put {
                            name: name.clone(),
                            namespace,
                            definition: Box::new(JobDefinition::from_spec(spec)),
                            trigger: Some(RunTrigger::Manual {
                                request_id: request_id.clone(),
                            }),
                            now_epoch_secs: *submitted_at_epoch_secs,
                        },
                        &mut *allocate,
                    )?;
                    let TaskArrayApplied::Registered { batch_id } = result else {
                        return Err(refuse("finite job produced no run"));
                    };
                    cohorts.push((name, batch_id));
                }
                let batch_id = allocate();
                self.manifests.insert(
                    batch_id,
                    TaskManifest {
                        name: format!(
                            "jobs-{}",
                            &hex::encode(Sha256::digest(request_id.as_bytes()))[..32]
                        ),
                        namespace: "default".into(),
                        cohorts,
                        common_jobs: true,
                        request: Some((request_id.clone(), digest)),
                        submitted_at_epoch_secs: *submitted_at_epoch_secs,
                    },
                );
                Ok(TaskArrayApplied::Registered { batch_id })
            }
            TaskArrayWrite::DeployBegin {
                operation_id,
                config,
                now_epoch_secs,
            } => {
                if operation_id.len() != 32
                    || !operation_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(refuse(
                        "deployment identity must be 32 hexadecimal characters",
                    ));
                }
                config
                    .validate_intrinsic()
                    .map_err(|error| TaskArrayStoreError::Job(error.to_string()))?;
                if serde_json::to_vec(config).map_or(true, |bytes| bytes.len() > 128 * 1024) {
                    return Err(refuse("deployment exceeds 128 KiB"));
                }
                if let Some(prior) = self.deployments.get(operation_id) {
                    return if prior.config == *config {
                        Ok(TaskArrayApplied::JobRecorded)
                    } else {
                        Err(refuse("deployment identity already names different work"))
                    };
                }
                if self
                    .deployments
                    .values()
                    .filter(|record| !record.completed)
                    .count()
                    >= 64
                {
                    return Err(refuse("64 deployments are already unsettled"));
                }
                for (name, namespace) in
                    config
                        .app
                        .iter()
                        .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default")))
                        .chain(config.job.iter().map(|(name, spec)| {
                            (name, spec.namespace.as_deref().unwrap_or("default"))
                        }))
                {
                    if self.deployment_owner(name, namespace).is_some() {
                        return Err(refuse("another deployment still owns this workload"));
                    }
                }
                if config.app.iter().any(|(name, spec)| {
                    self.job_identity_reserved(spec.namespace.as_deref().unwrap_or("default"), name)
                }) {
                    return Err(refuse(
                        "app identity belongs to a job definition or retained run",
                    ));
                }
                let mut prospective = self.clone();
                let mut preview_id = 0u64;
                for (name, spec) in &config.job {
                    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
                    let mut definition = JobDefinition::from_spec(spec.clone());
                    definition.template.namespace = Some(namespace.clone());
                    let trigger = definition.cron.is_none().then(|| RunTrigger::Hook {
                        operation_id: operation_id.clone(),
                    });
                    prospective.apply_job(
                        &JobWrite::Put {
                            name: name.clone(),
                            namespace,
                            definition: Box::new(definition),
                            trigger,
                            now_epoch_secs: *now_epoch_secs,
                        },
                        || {
                            preview_id += 1;
                            while self.arrays.contains_key(&preview_id)
                                || self.manifests.contains_key(&preview_id)
                            {
                                preview_id += 1;
                            }
                            preview_id
                        },
                    )?;
                }
                let terminal: Vec<_> = self
                    .deployments
                    .iter()
                    .filter(|(_, record)| record.completed)
                    .map(|(id, record)| (record.submitted_at_epoch_secs, id.clone()))
                    .collect();
                let mut terminal = terminal;
                terminal.sort();
                for (_, id) in terminal.iter().take(terminal.len().saturating_sub(19)) {
                    self.deployments.remove(id);
                }
                let mut record = DeploymentRecord {
                    config: config.clone(),
                    hook_runs: BTreeMap::new(),
                    job_runs: BTreeMap::new(),
                    apps_committed: false,
                    cancelled: false,
                    completed: false,
                    outcome: None,
                    finished_at: None,
                    submitted_at_epoch_secs: *now_epoch_secs,
                };
                // Ownership is recorded before prune can discard an earlier hook result.
                self.deployments
                    .insert(operation_id.clone(), record.clone());
                for (name, spec) in &config.job {
                    if spec.run_before.is_empty() {
                        continue;
                    }
                    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
                    let mut definition = JobDefinition::from_spec(spec.clone());
                    definition.template.namespace = Some(namespace.clone());
                    let result = self.apply_job(
                        &JobWrite::Put {
                            name: name.clone(),
                            namespace,
                            definition: Box::new(definition),
                            trigger: Some(RunTrigger::Hook {
                                operation_id: operation_id.clone(),
                            }),
                            now_epoch_secs: *now_epoch_secs,
                        },
                        &mut *allocate,
                    )?;
                    let TaskArrayApplied::Registered { batch_id } = result else {
                        return Err(refuse("hook admission produced no run"));
                    };
                    record.hook_runs.insert(name.clone(), batch_id);
                    self.deployments
                        .insert(operation_id.clone(), record.clone());
                }
                Ok(TaskArrayApplied::JobRecorded)
            }
            TaskArrayWrite::DeployCommitted {
                operation_id,
                now_epoch_secs,
            } => {
                let mut record = self
                    .deployments
                    .get(operation_id)
                    .cloned()
                    .ok_or_else(|| refuse("unknown deployment"))?;
                if record.cancelled {
                    return Err(refuse("cancelled deployment cannot publish apps"));
                }
                if record.apps_committed {
                    return Ok(TaskArrayApplied::JobRecorded);
                }
                if !record.hook_runs.values().all(|id| {
                    self.get(*id)
                        .is_some_and(|run| run.state.status() == TaskArrayStatus::Succeeded)
                }) {
                    return Err(refuse("hooks have no accepted successful outcome"));
                }
                // This is a preflighted candidate: release this operation's own
                // definition ownership before admitting its ordinary jobs and schedules.
                record.apps_committed = true;
                self.deployments
                    .insert(operation_id.clone(), record.clone());
                for (name, spec) in &record.config.job {
                    if !spec.run_before.is_empty() {
                        continue;
                    }
                    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
                    let mut definition = JobDefinition::from_spec(spec.clone());
                    definition.template.namespace = Some(namespace.clone());
                    let trigger = definition.cron.is_none().then(|| RunTrigger::Hook {
                        operation_id: operation_id.clone(),
                    });
                    let result = self.apply_job(
                        &JobWrite::Put {
                            name: name.clone(),
                            namespace,
                            definition: Box::new(definition),
                            trigger,
                            now_epoch_secs: *now_epoch_secs,
                        },
                        &mut *allocate,
                    )?;
                    if let TaskArrayApplied::Registered { batch_id } = result {
                        record.job_runs.insert(name.clone(), batch_id);
                    }
                    self.deployments
                        .insert(operation_id.clone(), record.clone());
                }
                record.apps_committed = true;
                self.deployments.insert(operation_id.clone(), record);
                Ok(TaskArrayApplied::JobRecorded)
            }
            TaskArrayWrite::DeployCancel {
                operation_id,
                now_epoch_secs,
            } => {
                let mut record = self
                    .deployments
                    .get(operation_id)
                    .cloned()
                    .ok_or_else(|| refuse("unknown deployment"))?;
                if record.completed {
                    return Ok(TaskArrayApplied::JobRecorded);
                }
                record.cancelled = true;
                for id in record.runs() {
                    self.record_mut(id)?.state.cancel();
                    self.mark_terminal(id, *now_epoch_secs)?;
                }
                self.deployments.insert(operation_id.clone(), record);
                Ok(TaskArrayApplied::Cancelled)
            }
            TaskArrayWrite::DeployRelease { operation_id } => {
                let record = self
                    .deployments
                    .get(operation_id)
                    .ok_or_else(|| refuse("unknown deployment"))?;
                if record.completed {
                    return Ok(TaskArrayApplied::JobRecorded);
                }
                let terminal = record.runs().all(|id| {
                    self.get(id)
                        .is_some_and(|run| run.state.status().is_terminal())
                });
                let failed = record.hook_runs.values().any(|id| {
                    self.get(*id).is_some_and(|run| {
                        run.state.status().is_terminal()
                            && run.state.status() != TaskArrayStatus::Succeeded
                    })
                });
                if !terminal || (!record.apps_committed && !record.cancelled && !failed) {
                    return Err(refuse("deployment has not positively settled"));
                }
                let any_failed = record.runs().any(|id| {
                    self.get(id).is_some_and(|run| {
                        matches!(
                            run.state.status(),
                            TaskArrayStatus::Failed | TaskArrayStatus::CompletedWithFailures
                        )
                    })
                });
                let finished_at = record
                    .runs()
                    .filter_map(|id| self.get(id).and_then(|run| run.terminal_at_epoch_secs))
                    .max()
                    .unwrap_or(record.submitted_at_epoch_secs);
                let outcome = if any_failed {
                    super::job_deploy::DeploymentOutcome::Failed
                } else if record.cancelled {
                    super::job_deploy::DeploymentOutcome::Cancelled
                } else {
                    super::job_deploy::DeploymentOutcome::Completed
                };
                if let Some(record) = self.deployments.get_mut(operation_id) {
                    record.completed = true;
                    record.outcome = Some(outcome);
                    record.finished_at = Some(finished_at);
                }
                Ok(TaskArrayApplied::JobRecorded)
            }
            _ => Err(refuse("not a deployment transaction")),
        }
    }

    /// Retained parent summaries, without enumerating any task.
    pub fn manifests(&self) -> impl Iterator<Item = (u64, &TaskManifest)> {
        self.manifests.iter().map(|(id, manifest)| (*id, manifest))
    }

    /// Look up a mixed-profile submission.
    pub fn manifest(&self, batch_id: u64) -> Option<&TaskManifest> {
        self.manifests.get(&batch_id)
    }

    /// Look up an array.
    pub fn get(&self, batch_id: u64) -> Option<&TaskArrayRecord> {
        self.arrays.get(&batch_id)
    }

    /// Every array, oldest id first.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &TaskArrayRecord)> {
        self.arrays.iter().map(|(id, record)| (*id, record))
    }

    /// Arrays that haven't reached a terminal status.
    pub fn active(&self) -> impl Iterator<Item = (u64, &TaskArrayRecord)> {
        self.iter()
            .filter(|(_, record)| !record.state.status().is_terminal())
    }

    /// Whether any array is still running or stopping.
    pub fn has_active(&self) -> bool {
        self.active().next().is_some()
    }

    /// Ids of every array still held (running or finished but retained).
    pub fn ids(&self) -> Vec<u64> {
        self.arrays.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::task_array::ChunkId;
    use crate::meat::task_array_state::TaskArrayStatus;

    fn template() -> Box<JobSpec> {
        Box::new(JobSpec {
            image: None,
            command: None,
            schedule: None,
            run_before: Vec::new(),
            memory: None,
            cpu: None,
            env: BTreeMap::new(),
            namespace: None,
            exec: Some("/usr/bin/true".into()),
            script: None,
        })
    }

    fn register(count: u32, chunk_size: u32, at: u64) -> TaskArrayWrite {
        TaskArrayWrite::Register {
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: template(),
            spec: TaskArraySpec {
                chunk_size,
                ..TaskArraySpec::with_count(count)
            },
            submitted_at_epoch_secs: at,
        }
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn counter() -> impl FnMut() -> u64 {
        let mut next = 0;
        move || {
            next += 1;
            next
        }
    }

    fn registered(arrays: &mut TaskArrays, write: &TaskArrayWrite, id: u64) -> u64 {
        match arrays.apply(write, || id).unwrap() {
            TaskArrayApplied::Registered { batch_id } => batch_id,
            other => panic!("expected a registration, got {other:?}"),
        }
    }

    fn finished(chunk: u32, attempt: u64, succeeded: u32) -> ChunkResult {
        ChunkResult {
            duration_counts: [0; 16],
            chunk: ChunkId(chunk),
            attempt,
            succeeded,
            failed_count: 0,
            failed_indices: IndexRangeSet::new(),
            not_run: 0,
            retried: 0,
        }
    }

    fn sync(
        batch_id: u64,
        results: Vec<(NodeId, ChunkResult)>,
        grants: Vec<(NodeId, IndexRangeSet)>,
    ) -> TaskArrayWrite {
        TaskArrayWrite::Sync {
            now_epoch_secs: 0,
            batch_id,
            results,
            grants,
        }
    }

    #[test]
    fn register_stores_the_array_under_the_allocated_id() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(10, 4, 100), 7);
        assert_eq!(id, 7);
        let record = arrays.get(7).unwrap();
        assert_eq!(record.name, "render");
        assert_eq!(record.state.spec.chunk_count(), 3);
        assert_eq!(record.state.submitted_at_epoch_secs, 100);
        assert!(arrays.has_active());
    }

    #[test]
    fn refused_registration_does_not_allocate_an_id() {
        let mut arrays = TaskArrays::default();
        let mut allocated = false;
        let result = arrays.apply(&register(0, 4, 1), || {
            allocated = true;
            1
        });
        assert!(matches!(result, Err(TaskArrayStoreError::Invalid(_))));
        assert!(!allocated);
    }

    #[test]
    fn register_accepts_an_image_template() {
        let mut arrays = TaskArrays::default();
        let mut image = template();
        image.exec = None;
        image.image = Some("alpine:3".to_string());
        let write = TaskArrayWrite::Register {
            name: "x".to_string(),
            namespace: "default".to_string(),
            template: image,
            spec: TaskArraySpec::with_count(3),
            submitted_at_epoch_secs: 1,
        };
        assert_eq!(
            arrays.apply(&write, || 1).unwrap(),
            TaskArrayApplied::Registered { batch_id: 1 }
        );
        assert_eq!(
            arrays.get(1).unwrap().template.image.as_deref(),
            Some("alpine:3")
        );
    }

    #[test]
    fn register_refuses_an_empty_name() {
        let mut arrays = TaskArrays::default();
        let write = TaskArrayWrite::Register {
            name: "  ".to_string(),
            namespace: "default".to_string(),
            template: template(),
            spec: TaskArraySpec::with_count(3),
            submitted_at_epoch_secs: 1,
        };
        assert_eq!(
            arrays.apply(&write, || 1),
            Err(TaskArrayStoreError::EmptyName)
        );
    }

    #[test]
    fn register_refuses_past_the_active_limit() {
        let mut arrays = TaskArrays::default();
        let mut ids = counter();
        for _ in 0..MAX_ACTIVE_ARRAYS {
            arrays.apply(&register(4, 4, 1), &mut ids).unwrap();
        }
        assert_eq!(
            arrays.apply(&register(4, 4, 1), &mut ids),
            Err(TaskArrayStoreError::TooManyActive {
                active: MAX_ACTIVE_ARRAYS
            })
        );
    }

    #[test]
    fn registration_prunes_finished_arrays_past_retention() {
        let mut arrays = TaskArrays::default();
        let old = registered(&mut arrays, &register(4, 4, 100), 1);
        arrays
            .apply(
                &TaskArrayWrite::Cancel {
                    now_epoch_secs: 0,
                    batch_id: old,
                },
                || 0,
            )
            .unwrap();
        assert!(arrays.get(old).unwrap().state.status().is_terminal());

        // Within the window it stays; past it, the next registration drops it.
        registered(
            &mut arrays,
            &register(4, 4, 100 + TERMINAL_RETENTION_SECS),
            2,
        );
        assert!(arrays.get(old).is_some());
        registered(
            &mut arrays,
            &register(4, 4, 101 + TERMINAL_RETENTION_SECS),
            3,
        );
        assert!(arrays.get(old).is_none());
    }

    #[test]
    fn registration_keeps_only_the_newest_finished_arrays() {
        let mut arrays = TaskArrays::default();
        for id in 1..=(MAX_TERMINAL_ARRAYS as u64 + 5) {
            registered(&mut arrays, &register(4, 4, 1), id);
            arrays
                .apply(
                    &TaskArrayWrite::Cancel {
                        now_epoch_secs: 0,
                        batch_id: id,
                    },
                    || 0,
                )
                .unwrap();
        }
        registered(&mut arrays, &register(4, 4, 1), 1000);
        let terminal: Vec<u64> = arrays
            .iter()
            .filter(|(_, r)| r.state.status().is_terminal())
            .map(|(id, _)| id)
            .collect();
        assert_eq!(terminal.len(), MAX_TERMINAL_ARRAYS);
        assert_eq!(terminal.first(), Some(&6), "the oldest go first");
    }

    #[test]
    fn sync_retires_results_before_granting() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(8, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        // One write both retires chunk 0 and grants chunk 1 to the same node.
        let applied = arrays
            .apply(
                &sync(
                    id,
                    vec![(node("a"), finished(0, 1, 4))],
                    vec![(node("a"), IndexRangeSet::from_range(1..=1))],
                ),
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Synced {
                results_applied: 1,
                results_refused: 0,
                grants_applied: 1,
                grants_refused: 0,
            }
        );
        let summary = arrays.get(id).unwrap().state.summary();
        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.held, 4);
    }

    #[test]
    fn sync_skips_stale_items_without_refusing_the_entry() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(8, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Requeue {
                    now_epoch_secs: 0,
                    batch_id: id,
                    node: node("a"),
                },
                || 0,
            )
            .unwrap();
        // "a"'s late report is fenced; granting a chunk already held fails
        // too; the good grant still lands.
        let applied = arrays
            .apply(
                &sync(
                    id,
                    vec![(node("a"), finished(0, 1, 4))],
                    vec![
                        (node("b"), IndexRangeSet::from_range(0..=0)),
                        (node("c"), IndexRangeSet::from_range(0..=0)),
                    ],
                ),
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Synced {
                results_applied: 0,
                results_refused: 1,
                grants_applied: 1,
                grants_refused: 1,
            }
        );
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.attempt_of(ChunkId(0)), 2);
        assert!(state.held_by(&node("b")).is_some());
    }

    #[test]
    fn raw_arrays_cannot_take_over_an_unsettled_deployment_name() {
        for manifest in [false, true] {
            let mut arrays = TaskArrays::default();
            let config = crate::config::Config::parse(
                "[app.web]\nimage='web:v1'\n[job.render]\nexec='/bin/true'\nrun_before=['app.web']",
            )
            .unwrap();
            let mut next = 1;
            arrays
                .apply(
                    &TaskArrayWrite::DeployBegin {
                        operation_id: "a".repeat(32),
                        config: Box::new(config),
                        now_epoch_secs: 1,
                    },
                    || {
                        let id = next;
                        next += 1;
                        id
                    },
                )
                .unwrap();
            let mut write = register(1, 1, 2);
            let TaskArrayWrite::Register { name, .. } = &mut write else {
                unreachable!()
            };
            *name = "render".into();
            if manifest {
                let TaskArrayWrite::Register {
                    name,
                    namespace,
                    template,
                    spec,
                    submitted_at_epoch_secs,
                } = write
                else {
                    unreachable!()
                };
                write = TaskArrayWrite::RegisterManifest {
                    name,
                    namespace,
                    cohorts: vec![ManifestCohort {
                        name: "small".into(),
                        template: *template,
                        spec,
                    }],
                    submitted_at_epoch_secs,
                };
            }
            let before = arrays.clone();
            assert!(
                arrays
                    .apply(&write, || {
                        let id = next;
                        next += 1;
                        id
                    })
                    .is_err()
            );
            assert_eq!(arrays, before);
            assert_eq!(next, 2, "refusal must not allocate an identity");
        }
    }

    #[test]
    fn writes_to_an_unknown_array_are_refused() {
        let mut arrays = TaskArrays::default();
        for write in [
            sync(9, vec![], vec![]),
            TaskArrayWrite::Cancel {
                now_epoch_secs: 0,
                batch_id: 9,
            },
            TaskArrayWrite::Requeue {
                now_epoch_secs: 0,
                batch_id: 9,
                node: node("a"),
            },
        ] {
            assert_eq!(
                arrays.apply(&write, || 0),
                Err(TaskArrayStoreError::UnknownArray { batch_id: 9 })
            );
        }
    }

    #[test]
    fn cancel_stops_the_queue_and_requeue_writes_off_held_chunks() {
        let mut arrays = TaskArrays::default();
        let id = registered(&mut arrays, &register(12, 4, 1), 1);
        arrays
            .apply(
                &sync(
                    id,
                    vec![],
                    vec![(node("a"), IndexRangeSet::from_range(0..=0))],
                ),
                || 0,
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Cancel {
                    now_epoch_secs: 0,
                    batch_id: id,
                },
                || 0,
            )
            .unwrap();
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.status(), TaskArrayStatus::Stopping);
        assert_eq!(state.summary().not_run, 8);

        let applied = arrays
            .apply(
                &TaskArrayWrite::Requeue {
                    now_epoch_secs: 0,
                    batch_id: id,
                    node: node("a"),
                },
                || 0,
            )
            .unwrap();
        assert_eq!(
            applied,
            TaskArrayApplied::Requeued {
                chunks: IndexRangeSet::from_range(0..=0)
            }
        );
        let state = &arrays.get(id).unwrap().state;
        assert_eq!(state.status(), TaskArrayStatus::Cancelled);
        assert!(!arrays.has_active());
    }

    #[test]
    fn writes_and_the_store_round_trip_through_json() {
        let mut arrays = TaskArrays::default();
        let write = register(10, 4, 5);
        let json = serde_json::to_string(&write).unwrap();
        let back: TaskArrayWrite = serde_json::from_str(&json).unwrap();
        assert_eq!(back, write);
        registered(&mut arrays, &back, 3);
        let json = serde_json::to_string(&arrays).unwrap();
        let back: TaskArrays = serde_json::from_str(&json).unwrap();
        assert_eq!(back, arrays);
    }
    #[test]
    fn manifest_retention_starts_at_last_completion_and_keeps_profiles_together() {
        let mut arrays = TaskArrays::default();
        let mut next = 1;
        let cohort = |name: &str| ManifestCohort {
            name: name.into(),
            spec: TaskArraySpec {
                chunk_size: 1,
                ..TaskArraySpec::with_count(1)
            },
            template: *template(),
        };
        arrays
            .apply(
                &TaskArrayWrite::RegisterManifest {
                    name: "mixed".into(),
                    namespace: "default".into(),
                    cohorts: vec![cohort("small"), cohort("large")],
                    submitted_at_epoch_secs: 0,
                },
                || {
                    let id = next;
                    next += 1;
                    id
                },
            )
            .unwrap();
        let n = NodeId::new("n1");
        arrays
            .apply(
                &sync(
                    2,
                    vec![],
                    vec![(n.clone(), IndexRangeSet::from_range(0..=0))],
                ),
                || panic!("sync allocated ID"),
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Sync {
                    batch_id: 2,
                    now_epoch_secs: 100,
                    results: vec![(n.clone(), finished(0, 1, 1))],
                    grants: vec![],
                },
                || unreachable!(),
            )
            .unwrap();
        registered(&mut arrays, &register(1, 1, 4200), 10);
        assert!(
            arrays.get(2).is_some(),
            "early profile pruned while parent was active"
        );
        arrays
            .apply(
                &sync(
                    3,
                    vec![],
                    vec![(n.clone(), IndexRangeSet::from_range(0..=0))],
                ),
                || unreachable!(),
            )
            .unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Sync {
                    batch_id: 3,
                    now_epoch_secs: 5000,
                    results: vec![(n, finished(0, 1, 1))],
                    grants: vec![],
                },
                || unreachable!(),
            )
            .unwrap();
        registered(&mut arrays, &register(1, 1, 8500), 11);
        assert!(arrays.manifest(1).is_some());
        assert!(arrays.get(2).is_some());
        registered(&mut arrays, &register(1, 1, 8601), 12);
        assert!(arrays.manifest(1).is_none());
        assert!(arrays.get(2).is_none());
        assert!(arrays.get(3).is_none());
    }
}

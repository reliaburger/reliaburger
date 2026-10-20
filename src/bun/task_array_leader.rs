//! The leader's side of task arrays: one loop that talks to every node
//! once a second and turns what they say into one Raft entry per tick.
//!
//! Each tick the leader reads the replicated [`TaskArrays`], sends every
//! node its share (the chunks it holds, at their attempts) and collects
//! finished chunks and free slots until the tick's deadline
//! ([`sync_deadline`]). A node that hasn't answered by then keeps its call
//! running into the next tick ([`SyncCalls`]), and isn't called again until
//! it answers, so one slow node never holds up grants to the others.
//!
//! [`plan_tick`] then turns the answers into one batch of writes for every
//! running array: a `Sync` with the finished chunks and new grants planned
//! with [`plan_grants`] against the state as it will be once those chunks
//! are retired, plus a `Requeue` for any node that holds chunks but hasn't
//! answered for [`SILENCE_TIMEOUT`], which hands its chunks to others at the
//! next attempt so a late report from it is fenced off. The batch commits
//! as a single `Tick` entry, however many arrays are running.
//!
//! Production persists the same state through a council or the private
//! standalone job store. Tests may explicitly select volatile standalone state.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::api::ApiState;
use super::task_array_node::{
    ArrayAssignment, ArrayProgress, HeldChunk, NodeArrayCounters, NodeSyncRequest,
    NodeSyncResponse, TaskArrayNode,
};
use crate::config::types::EnvValue;
use crate::council::types::{CouncilResponse, RaftRequest};
use crate::meat::NodeId;
use crate::meat::task_array::ChunkId;
use crate::meat::task_array_state::{FittedGrants, NodeSlots, fit_grants, plan_grants};
use crate::meat::task_array_store::{
    MAX_TICK_WRITES, TaskArrayApplied, TaskArrayRecord, TaskArrayWrite, TaskArrays,
};

/// How often the leader syncs every node.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// How long a node holding chunks may go without answering before the
/// leader takes them back.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one node's sync call may take.
pub const NODE_SYNC_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a tick waits for the nodes' answers before it plans grants
/// from the ones it has: half the sync interval, so a node that hangs
/// costs the others nothing and the tick still ends well inside its slot.
pub fn sync_deadline(interval: Duration) -> Duration {
    interval / 2
}

/// How long a task-array Raft write may take before the caller gives up
/// (the entry may still commit; every write is safe to repeat or drop).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the leader reads the time it fires cron schedules at.
///
/// Production always uses [`CronClock::System`]. A test that needs an
/// occurrence to come round at an exact point, say after it has filled the
/// cluster with other work, holds a [`CronClock::Manual`] still and moves it
/// on with [`CronClock::set`], instead of racing the wall clock (#748).
#[derive(Debug, Clone, Default)]
pub enum CronClock {
    /// The host's UTC wall clock.
    #[default]
    System,
    /// Seconds since the Unix epoch, shared by every clone and changed only
    /// by [`CronClock::set`].
    Manual(Arc<std::sync::atomic::AtomicU64>),
}

impl CronClock {
    /// A manual clock that stands at `epoch_secs` until it is set.
    pub fn manual(epoch_secs: u64) -> Self {
        Self::Manual(Arc::new(std::sync::atomic::AtomicU64::new(epoch_secs)))
    }

    /// Move a manual clock, and every clone of it, to `epoch_secs`. The
    /// system clock can't be moved, so this does nothing to it.
    pub fn set(&self, epoch_secs: u64) {
        if let Self::Manual(time) = self {
            time.store(epoch_secs, std::sync::atomic::Ordering::Release);
        }
    }

    /// Seconds since the Unix epoch, by this clock.
    pub fn now_epoch_secs(&self) -> u64 {
        match self {
            Self::System => crate::meat::batch_ids::epoch_now_secs(),
            Self::Manual(time) => time.load(std::sync::atomic::Ordering::Acquire),
        }
    }
}

/// The latest word from one node about one array, for status views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeView {
    /// The node.
    pub node: NodeId,
    /// Slots it offered.
    pub slots: u32,
    /// Why it can't run the array, if it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// Its counters.
    pub counters: NodeArrayCounters,
}

/// Task-array settings, the node executor and the leader's in-memory
/// view, shared by the API handlers and the leader loop.
pub struct TaskArrayService {
    /// Standalone app publication is serialised with admission and cancellation.
    pub(crate) apply_gate: Arc<Mutex<()>>,
    pub(crate) publishing: Mutex<std::collections::BTreeSet<String>>,
    /// Image signature policy applied before admission and dispatch.
    pub trust_policy: crate::config::node::TrustPolicySection,
    /// Registry client shared with normal job admission for cosign checks.
    pub signature_source: Option<crate::pickle::cosign::SignatureSource>,
    /// This node's executor; `None` means this node can't run tasks
    /// (it still coordinates when it leads).
    pub node: Option<Arc<TaskArrayNode>>,
    /// Unconfigured production services refuse volatile standalone admission.
    require_council: bool,
    standalone: Option<Arc<std::sync::Mutex<super::job_store::JobStore>>>,
    storage_fenced: std::sync::atomic::AtomicBool,
    /// How long a standalone publication may take before its caller gives
    /// up and fences admission and dispatch: [`WRITE_TIMEOUT`] outside tests.
    write_timeout: Duration,
    /// Time between syncs.
    pub sync_interval: Duration,
    /// Silence before a node's chunks are taken back.
    pub silence_timeout: Duration,
    /// How long a finished run stays readable before the leader prunes it.
    pub retention: Duration,
    /// The time cron schedules fire at.
    cron_clock: CronClock,
    /// Standalone copy of the arrays (unused with a council).
    local: Mutex<TaskArrays>,
    /// The id counter for `local`. With a council the counter lives in Raft.
    local_ids: Mutex<crate::meat::batch_ids::BatchIds>,
    /// What each node last said about each array, by batch id.
    views: Mutex<HashMap<u64, BTreeMap<NodeId, NodeView>>>,
    rates: Mutex<HashMap<u64, super::task_rates::RateSample>>,
    /// Where the leader sends failed and unknown-outcome run notifications.
    pub(crate) job_notifier: Option<crate::mayo::webhook::WebhookDispatcher>,
}

impl TaskArrayService {
    /// A service with the production timings.
    pub fn new(node: Option<Arc<TaskArrayNode>>) -> Self {
        let mut service = Self::with_timings(node, SYNC_INTERVAL, SILENCE_TIMEOUT);
        service.require_council = true;
        service
    }

    /// A production standalone coordinator must have durable storage configured.
    pub(crate) fn admission_configured(&self) -> bool {
        !self.require_council || self.standalone.is_some()
    }

    /// Apply the configured image policy to batch admission too.
    pub fn with_trust_policy(mut self, policy: crate::config::node::TrustPolicySection) -> Self {
        self.trust_policy = policy;
        self
    }

    /// Send failed and unknown-outcome run notifications to these alert
    /// destinations while this node leads. Nothing is sent when it has none.
    pub fn with_job_notifier(mut self, notifier: crate::mayo::webhook::WebhookDispatcher) -> Self {
        self.job_notifier = notifier.has_destinations().then_some(notifier);
        self
    }

    pub fn with_signature_source(
        mut self,
        source: Option<crate::pickle::cosign::SignatureSource>,
    ) -> Self {
        self.signature_source = source;
        self
    }

    /// Test harness service with explicit timings and volatile standalone state.
    /// Production adds durable storage with `with_storage` or uses a council.
    pub fn with_timings(
        node: Option<Arc<TaskArrayNode>>,
        sync_interval: Duration,
        silence_timeout: Duration,
    ) -> Self {
        Self {
            apply_gate: Arc::new(Mutex::new(())),
            publishing: Mutex::new(Default::default()),
            trust_policy: Default::default(),
            signature_source: None,
            require_council: false,
            standalone: None,
            storage_fenced: std::sync::atomic::AtomicBool::new(false),
            write_timeout: WRITE_TIMEOUT,
            node,
            sync_interval,
            silence_timeout,
            retention: Duration::from_secs(crate::meat::task_array_store::TERMINAL_RETENTION_SECS),
            cron_clock: CronClock::System,
            local: Mutex::new(TaskArrays::default()),
            local_ids: Mutex::new(crate::meat::batch_ids::BatchIds::default()),
            views: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            job_notifier: None,
        }
    }

    /// Keep finished runs readable for `retention` instead of the default
    /// hour. Tests use a short window to watch collection happen.
    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.retention = retention;
        self
    }

    /// Fire cron schedules at `clock`'s time instead of the wall clock's.
    /// Tests use a manual clock to say exactly when an occurrence comes round.
    pub fn with_cron_clock(mut self, clock: CronClock) -> Self {
        self.cron_clock = clock;
        self
    }

    /// Give up on a standalone publication after `timeout`, so a test can
    /// expire its caller without waiting the production [`WRITE_TIMEOUT`].
    #[cfg(test)]
    pub(crate) fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Hold the durable store's next publication before it writes, until
    /// `pause` is released. Nothing happens without durable storage.
    #[cfg(test)]
    pub(crate) fn pause_next_publication(&self, pause: Arc<std::sync::Barrier>) {
        if let Some(store) = &self.standalone {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pause_next_publication(pause);
        }
    }

    /// Open private durable standalone state before admitting or dispatching work.
    pub async fn with_storage(mut self, data: &std::path::Path) -> std::io::Result<Self> {
        let directory = data.join("job-state");
        let store =
            tokio::task::spawn_blocking(move || super::job_store::JobStore::open(&directory))
                .await
                .map_err(std::io::Error::other)??;
        self.standalone = Some(Arc::new(std::sync::Mutex::new(store)));
        self.require_council = false;
        Ok(self)
    }

    /// Rate of unique accepted results; first or stale samples are unknown.
    pub async fn rates(&self, batch_id: u64, counts: (u64, u64)) -> super::task_rates::TaskRates {
        let now = Instant::now();
        let epoch_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        self.rates
            .lock()
            .await
            .entry(batch_id)
            .or_insert_with(|| super::task_rates::RateSample::new(counts, now, epoch_ms))
            .update(counts, now, epoch_ms)
    }

    /// What each node last said about `batch_id`.
    pub async fn node_views(&self, batch_id: u64) -> Vec<NodeView> {
        self.views
            .lock()
            .await
            .get(&batch_id)
            .map(|views| views.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// Why a task-array write didn't happen.
#[derive(Debug, thiserror::Error)]
pub enum TaskArrayWriteError {
    /// The state machine refused it (invalid, unknown array, limits).
    #[error("{0}")]
    Refused(String),
    /// The cluster couldn't commit it.
    #[error("cluster state unavailable: {0}")]
    Unavailable(String),
}

/// What a committed write produced, as far as its caller cares.
struct Committed {
    /// The new id, for a registration.
    registered: Option<u64>,
    /// A tick's refused writes, by array.
    refused: Vec<(u64, String)>,
}

impl From<TaskArrayApplied> for Committed {
    fn from(applied: TaskArrayApplied) -> Self {
        match applied {
            TaskArrayApplied::Registered { batch_id } => Self {
                registered: Some(batch_id),
                refused: Vec::new(),
            },
            TaskArrayApplied::Ticked { refused } => Self {
                registered: None,
                refused,
            },
            _ => Self {
                registered: None,
                refused: Vec::new(),
            },
        }
    }
}

/// Commit a write: through Raft with a council, in memory standalone.
/// Returns the new id for a registration.
pub(crate) async fn write_task_array(
    state: &ApiState,
    write: TaskArrayWrite,
) -> Result<Option<u64>, TaskArrayWriteError> {
    Ok(commit_task_array(state, write).await?.registered)
}

/// Group one tick's progress writes into `Tick` entries: one, unless there
/// are more than [`MAX_TICK_WRITES`].
pub fn tick_entries(writes: Vec<TaskArrayWrite>) -> Vec<TaskArrayWrite> {
    let mut entries = Vec::new();
    let mut writes = writes.into_iter().peekable();
    while writes.peek().is_some() {
        entries.push(TaskArrayWrite::Tick {
            writes: writes.by_ref().take(MAX_TICK_WRITES).collect(),
        });
    }
    entries
}

/// Commit one tick's progress writes (see [`tick_entries`]). Returns the
/// writes the state machine refused, by array; the rest applied.
pub(crate) async fn write_tick(
    state: &ApiState,
    writes: Vec<TaskArrayWrite>,
) -> Result<Vec<(u64, String)>, TaskArrayWriteError> {
    let mut refused = Vec::new();
    for entry in tick_entries(writes) {
        refused.extend(commit_task_array(state, entry).await?.refused);
    }
    Ok(refused)
}

async fn commit_task_array(
    state: &ApiState,
    mut write: TaskArrayWrite,
) -> Result<Committed, TaskArrayWriteError> {
    write.stamp(crate::meat::batch_ids::epoch_now_secs());
    let Some(council) = &state.council else {
        return commit_standalone(&state.task_arrays, write).await;
    };
    let written = tokio::time::timeout(
        WRITE_TIMEOUT,
        council.write(RaftRequest::TaskArray(Box::new(write))),
    )
    .await
    .map_err(|_| TaskArrayWriteError::Unavailable("the Raft write timed out".to_string()))?;
    match written {
        Ok(CouncilResponse::TaskArrayRegistered { batch_id }) => {
            Ok(TaskArrayApplied::Registered { batch_id }.into())
        }
        Ok(CouncilResponse::TaskArrayTicked { refused }) => {
            Ok(TaskArrayApplied::Ticked { refused }.into())
        }
        Ok(CouncilResponse::Refused { reason }) => Err(TaskArrayWriteError::Refused(reason)),
        Ok(_) => Ok(TaskArrayApplied::JobRecorded.into()),
        Err(error) => Err(TaskArrayWriteError::Unavailable(error.to_string())),
    }
}

/// Commit a write to a standalone node's own arrays. Returns the new id for
/// a registration.
pub(crate) async fn write_standalone(
    service: &TaskArrayService,
    write: TaskArrayWrite,
) -> Result<Option<u64>, TaskArrayWriteError> {
    Ok(commit_standalone(service, write).await?.registered)
}

async fn commit_standalone(
    service: &TaskArrayService,
    mut write: TaskArrayWrite,
) -> Result<Committed, TaskArrayWriteError> {
    write.stamp(crate::meat::batch_ids::epoch_now_secs());
    if service.require_council {
        return Err(TaskArrayWriteError::Unavailable(
            "delegated arrays require a council for durable definitions and identities".into(),
        ));
    }
    if let Some(store) = &service.standalone {
        if service
            .storage_fenced
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(TaskArrayWriteError::Unavailable(
                "standalone publication timed out; restart and reconcile".into(),
            ));
        }
        let store = Arc::clone(store);
        let publication = tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| TaskArrayWriteError::Unavailable("job store lock poisoned".into()))?;
            store.apply(&write).map(Committed::from).map_err(|error| {
                if store.ready() {
                    TaskArrayWriteError::Refused(error)
                } else {
                    TaskArrayWriteError::Unavailable(error)
                }
            })
        });
        return match tokio::time::timeout(service.write_timeout, publication).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                service
                    .storage_fenced
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(TaskArrayWriteError::Unavailable(error.to_string()))
            }
            Err(_) => {
                service
                    .storage_fenced
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(TaskArrayWriteError::Unavailable(
                    "standalone publication timed out; admission and dispatch are fenced".into(),
                ))
            }
        };
    }
    let mut batch_ids = service.local_ids.lock().await;
    let mut arrays = service.local.lock().await;
    let ids = arrays
        .planned_ids(&write)
        .map_err(|error| TaskArrayWriteError::Refused(error.to_string()))?;
    batch_ids
        .preflight(ids)
        .map_err(TaskArrayWriteError::Refused)?;
    arrays
        .apply(&write, || batch_ids.allocate())
        .map(Committed::from)
        .map_err(|error| TaskArrayWriteError::Refused(error.to_string()))
}

/// A standalone node's arrays.
pub(crate) async fn read_standalone(service: &TaskArrayService) -> TaskArrays {
    if let Some(store) = &service.standalone {
        let store = Arc::clone(store);
        return tokio::task::spawn_blocking(move || {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .arrays()
                .clone()
        })
        .await
        .unwrap_or_default();
    }
    service.local.lock().await.clone()
}

/// The current arrays, copied: the replicated copy with a council, the
/// local one standalone. Prefer [`read_control`] where a borrow will do.
pub(crate) async fn read_task_arrays(state: &ApiState) -> TaskArrays {
    read_control(state, |view| view.arrays.clone())
        .await
        .unwrap_or_default()
}

/// What the leader reads from the job control state, borrowed in place.
pub(crate) struct ControlView<'a> {
    /// Every array the cluster keeps.
    pub arrays: &'a TaskArrays,
    /// The council's whole desired state (`None` standalone).
    pub desired: Option<&'a crate::council::types::DesiredState>,
    /// The standalone store's revision (0 with a council or volatile state).
    pub revision: u64,
}

/// Run `read` against the job control state under its lock, without
/// copying it. `None` only if the standalone store's reader failed.
pub(crate) async fn read_control<T: Send + 'static>(
    state: &ApiState,
    read: impl FnOnce(ControlView<'_>) -> T + Send + 'static,
) -> Option<T> {
    if let Some(council) = &state.council {
        return Some(
            council
                .read_desired(|desired| {
                    read(ControlView {
                        arrays: &desired.task_arrays,
                        desired: Some(desired),
                        revision: 0,
                    })
                })
                .await,
        );
    }
    if let Some(store) = &state.task_arrays.standalone {
        let store = Arc::clone(store);
        return tokio::task::spawn_blocking(move || {
            let store = store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            read(ControlView {
                arrays: store.arrays(),
                desired: None,
                revision: store.revision(),
            })
        })
        .await
        .ok();
    }
    let arrays = state.task_arrays.local.lock().await;
    Some(read(ControlView {
        arrays: &arrays,
        desired: None,
        revision: 0,
    }))
}

/// What one attempt of `template` reserves: its CPU and memory requests,
/// or 1 CPU and 64 MiB when it names none. Nodes admit attempts against
/// it, and namespace quotas charge it.
pub fn task_reservation(template: &crate::config::job::JobSpec) -> crate::meat::Resources {
    crate::meat::Resources::new(
        template.cpu.map_or(1000, |r| r.request),
        template.memory.map_or(64 << 20, |r| r.request),
        0,
    )
}

/// What one node should be told about one array: its held chunks with
/// their attempts, and how to run a task.
pub fn assignment_for(batch_id: u64, record: &TaskArrayRecord, node: &NodeId) -> ArrayAssignment {
    let state = &record.state;
    let held = state
        .held_by(node)
        .map(|chunks| {
            chunks
                .iter()
                .map(|chunk| HeldChunk {
                    chunk: ChunkId(chunk),
                    attempt: state.attempt_of(ChunkId(chunk)),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut template = record.template.clone();
    template.namespace = Some(record.namespace.clone());
    ArrayAssignment {
        template: Some(Box::new(template)),
        batch_id,
        resources: task_reservation(&record.template),
        spec: state.spec.clone(),
        program: record.template.exec.clone().unwrap_or_else(|| {
            if record.template.script.is_some() {
                "/bin/sh".into()
            } else {
                Default::default()
            }
        }),
        args: record
            .template
            .script
            .as_ref()
            .map(|script| vec!["-c".into(), script.clone()])
            .unwrap_or_else(|| record.template.command.clone().unwrap_or_default()),
        env: record
            .template
            .env
            .iter()
            .filter_map(|(key, value)| match value {
                EnvValue::Plain(value) => Some((key.clone(), value.clone())),
                // The owned runner decrypts the original template at execution.
                EnvValue::Encrypted(_) => None,
            })
            .chain(std::iter::once((
                "RELIABURGER_JOB_NAME".into(),
                record.name.clone(),
            )))
            .collect(),
        held,
        stopping: state.stop_reason().is_some(),
        replay_unknown: true,
        labels: crate::mayo::job_metrics::JobLabels {
            namespace: record.namespace.clone(),
            definition: record.name.clone(),
            profile: DEFAULT_PROFILE.into(),
        },
    }
}

/// The `profile` label of a run that isn't one profile of a manifest.
pub const DEFAULT_PROFILE: &str = "default";

/// The whole sync call for one node: every running array, plus the ids of
/// every array the cluster still keeps.
pub fn sync_request_for(arrays: &TaskArrays, node: &NodeId) -> NodeSyncRequest {
    let profiles: HashMap<u64, &str> = arrays
        .manifests()
        .flat_map(|(_, manifest)| {
            manifest
                .cohorts
                .iter()
                .map(|(profile, id)| (*id, profile.as_str()))
        })
        .collect();
    NodeSyncRequest {
        version: Default::default(),
        known: arrays.ids(),
        arrays: arrays
            .active()
            .map(|(batch_id, record)| {
                let mut assignment = assignment_for(batch_id, record, node);
                assignment.replay_unknown = arrays
                    .jobs()
                    .run(batch_id)
                    .is_none_or(|run| run.replay_unknown);
                if let Some(profile) = profiles.get(&batch_id) {
                    assignment.labels.profile = (*profile).to_string();
                }
                assignment
            })
            .collect(),
    }
}

/// Turn the nodes' answers about one array into its single `Sync` entry
/// for this tick, or `None` if nothing changed. Only results for chunks
/// the node still holds at that attempt are included; grants are planned
/// as if those results had already been applied, so a node that just
/// finished a chunk gets the next one in the same entry.
/// `replay_unknown` is the run's policy: without it, grants keep the small
/// window (see [`plan_grants`]).
pub fn plan_sync(
    batch_id: u64,
    record: &TaskArrayRecord,
    answers: &[(NodeId, &ArrayProgress)],
    replay_unknown: bool,
) -> Option<TaskArrayWrite> {
    plan_sync_within(batch_id, record, answers, replay_unknown, u64::MAX).0
}

/// [`plan_sync`], with grants trimmed so they start at most `room` more
/// attempts (see [`fit_grants`]). Also says what the grants start and
/// what the quota held back.
pub fn plan_sync_within(
    batch_id: u64,
    record: &TaskArrayRecord,
    answers: &[(NodeId, &ArrayProgress)],
    replay_unknown: bool,
    room: u64,
) -> (Option<TaskArrayWrite>, FittedGrants) {
    // Copy the state only if some chunk finished: most ticks for most
    // arrays only top nodes up, and that plans against the state as it is.
    let mut preview = std::borrow::Cow::Borrowed(&record.state);
    let mut results = Vec::new();
    for (node, progress) in answers {
        for result in &progress.finished {
            if !record.state.done().contains(result.chunk.0)
                && preview.to_mut().complete(node, result).is_ok()
            {
                results.push((node.clone(), result.clone()));
            }
        }
    }
    let slots: Vec<NodeSlots> = answers
        .iter()
        .map(|(node, progress)| NodeSlots {
            node: node.clone(),
            slots: progress.slots,
        })
        .collect();
    let planned = plan_grants(&preview, &slots, replay_unknown);
    let mut fitted = fit_grants(&preview, planned, &slots, room);
    if results.is_empty() && fitted.grants.is_empty() {
        return (None, fitted);
    }
    let write = TaskArrayWrite::Sync {
        now_epoch_secs: 0,
        batch_id,
        results,
        grants: std::mem::take(&mut fitted.grants),
    };
    (Some(write), fitted)
}

/// Namespace quotas as one leader tick's grants see them (D20). App
/// placements are charged first and every job attempt already in flight
/// next, so jobs only use what apps leave; then each run's new grants take
/// what's left, oldest run first. Built only when some namespace has a quota.
pub struct GrantBudget {
    ledger: crate::meat::quota::QuotaLedger,
}

impl GrantBudget {
    /// The budget for this tick, or `None` when no namespace declares a
    /// quota. `slots` is what a node last said about an array, if it answered.
    pub fn new<'a>(
        namespaces: &std::collections::BTreeMap<String, crate::config::NamespaceSpec>,
        placements: impl IntoIterator<
            Item = (
                &'a crate::meat::AppId,
                &'a Vec<crate::meat::types::Placement>,
            ),
        >,
        arrays: &TaskArrays,
        slots: impl Fn(u64, &NodeId) -> Option<u32>,
    ) -> Option<Self> {
        let mut ledger = crate::meat::quota::ledger_from_namespaces(namespaces);
        if ledger.is_empty() {
            return None;
        }
        for (app, placed) in placements {
            for placement in placed {
                ledger.charge_tasks(&app.namespace, &placement.resources, 1);
            }
        }
        for (batch_id, record) in arrays.active() {
            // A node that didn't answer could run every task it holds.
            let tasks = record.state.holders().fold(0u64, |sum, node| {
                let slots = slots(batch_id, node).unwrap_or(u32::MAX);
                sum.saturating_add(record.state.in_flight_on(node, slots))
            });
            ledger.charge_tasks(
                &record.namespace,
                &task_reservation(&record.template),
                tasks,
            );
        }
        Some(Self { ledger })
    }

    /// Plan one run's sync within its namespace's remaining quota, charge
    /// what it starts, and say why grants were held back, if they were.
    pub fn plan(
        &mut self,
        batch_id: u64,
        record: &TaskArrayRecord,
        answers: &[(NodeId, &ArrayProgress)],
        replay_unknown: bool,
    ) -> (
        Option<TaskArrayWrite>,
        Option<crate::meat::quota::QuotaError>,
    ) {
        let per_task = task_reservation(&record.template);
        let room = self.ledger.room_for_tasks(&record.namespace, &per_task);
        let (write, fitted) = plan_sync_within(batch_id, record, answers, replay_unknown, room);
        self.ledger
            .charge_tasks(&record.namespace, &per_task, fitted.started);
        let blocked = fitted.held_back.and_then(|tasks| {
            self.ledger
                .task_quota_error(&record.namespace, &per_task, tasks)
        });
        (write, blocked)
    }
}

/// The write that records a run's new quota-blocked reason, if it changed.
/// Usage numbers move with every task, so only a different limit (or none)
/// counts as a change.
pub fn quota_blocked_write(
    batch_id: u64,
    record: &TaskArrayRecord,
    blocked: Option<crate::meat::quota::QuotaError>,
) -> Option<TaskArrayWrite> {
    let same = match (&record.quota_blocked, &blocked) {
        (None, None) => true,
        (Some(old), Some(new)) => crate::meat::quota::same_limit(old, new),
        _ => false,
    };
    (!same).then_some(TaskArrayWrite::QuotaBlocked {
        batch_id,
        reason: blocked,
    })
}

/// Most cron occurrences the leader fires in one tick.
pub const MAX_FIRES_PER_TICK: usize = 16;

/// One schedule due now: its namespace, name and definition revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueSchedule {
    /// Namespace of the definition.
    pub namespace: String,
    /// Definition name.
    pub name: String,
    /// Revision the occurrence is fenced to.
    pub revision: u64,
}

/// The schedules that match `at` (UTC minute `minute`) and haven't fired
/// for it yet, at most [`MAX_FIRES_PER_TICK`]. The list starts after `cursor` (the last key fired) and
/// wraps round, so a long list takes turns instead of starving its tail.
pub fn due_schedules(
    arrays: &TaskArrays,
    at: time::OffsetDateTime,
    minute: i64,
    cursor: Option<&str>,
) -> Vec<DueSchedule> {
    let due: Vec<DueSchedule> = arrays
        .jobs()
        .definitions()
        .filter(|(_, record)| {
            record.last_observed_minute.is_none_or(|seen| seen < minute)
                && record.definition.cron.as_ref().is_some_and(|cron| {
                    crate::meat::cron::CronSchedule::parse(&cron.expression)
                        .is_ok_and(|schedule| schedule.matches(at))
                })
        })
        .filter_map(|(key, record)| {
            let (namespace, name) = key.split_once('/')?;
            Some(DueSchedule {
                namespace: namespace.to_string(),
                name: name.to_string(),
                revision: record.revision,
            })
        })
        .collect();
    let start = cursor.map_or(0, |cursor| {
        due.partition_point(|due| format!("{}/{}", due.namespace, due.name).as_str() <= cursor)
    });
    due.iter()
        .cycle()
        .skip(start)
        .take(due.len().min(MAX_FIRES_PER_TICK))
        .cloned()
        .collect()
}

/// Holders of `record`'s chunks that haven't answered in `silence`.
/// `last_heard` is when each node last answered; a node never heard from
/// counts from `since` (when this leader started leading).
pub fn silent_holders(
    record: &TaskArrayRecord,
    last_heard: &HashMap<NodeId, Instant>,
    since: Instant,
    now: Instant,
    silence: Duration,
) -> Vec<NodeId> {
    record
        .state
        .holders()
        .filter(|node| {
            let heard = last_heard.get(*node).copied().unwrap_or(since).max(since);
            now.saturating_duration_since(heard) > silence
        })
        .cloned()
        .collect()
}

/// Holders that answered but say they can't run the array (its binary
/// left their allowlist, or the ledger won't open). Their chunks would
/// never finish, so they go back to the queue like a silent node's.
pub fn refusing_holders(
    record: &TaskArrayRecord,
    answers: &[(NodeId, &ArrayProgress)],
) -> Vec<NodeId> {
    answers
        .iter()
        .filter(|(node, progress)| {
            progress.refused.is_some() && record.state.held_by(node).is_some()
        })
        .map(|(node, _)| node.clone())
        .collect()
}

/// Most distinct node reasons an unrunnable run's reason quotes.
const MAX_UNRUNNABLE_REASONS: usize = 3;

/// Why `record` should fail now because no node can run it, or `None`
/// while some node still might. That is when nothing holds its chunks,
/// something is still queued, and every node in `eligible` answered about
/// it this tick with a refusal from its own configuration
/// ([`ArrayProgress::unrunnable`]), such as a host array on nodes with
/// `mount_isolation` on (#714). Without this the run would stay queued
/// until its deadline, or for ever if it has none.
pub fn unrunnable_reason(
    record: &TaskArrayRecord,
    answers: &[(NodeId, &ArrayProgress)],
    eligible: &BTreeSet<NodeId>,
) -> Option<String> {
    if eligible.is_empty()
        || record.state.stop_reason().is_some()
        || record.state.queued().is_empty()
        || record.state.holders().next().is_some()
    {
        return None;
    }
    let mut reasons = BTreeSet::new();
    for node in eligible {
        let (_, progress) = answers.iter().find(|(answered, _)| answered == node)?;
        if !progress.unrunnable {
            return None;
        }
        reasons.insert(progress.refused.as_deref()?);
    }
    Some(format!("no node can run it: {}", quote_reasons(&reasons)))
}

/// The distinct reasons nodes gave for refusing a run, joined for a user:
/// at most [`MAX_UNRUNNABLE_REASONS`] of them, each cut to 256 bytes.
fn quote_reasons(reasons: &BTreeSet<&str>) -> String {
    let mut quoted: Vec<String> = reasons
        .iter()
        .take(MAX_UNRUNNABLE_REASONS)
        .map(|reason| {
            // Keep the write under the store's limit whatever a node says.
            let mut end = reason.len().min(256);
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason[..end].to_string()
        })
        .collect();
    if reasons.len() > MAX_UNRUNNABLE_REASONS {
        quoted.push(format!(
            "and {} more",
            reasons.len() - MAX_UNRUNNABLE_REASONS
        ));
    }
    quoted.join("; ")
}

/// A live node as the leader sees it when it admits a job run (#788).
pub struct AdvertisedNode<'a> {
    /// The node.
    pub node: NodeId,
    /// The kinds of job it advertises over gossip, or `None` if the leader
    /// hasn't heard.
    pub jobs: Option<crate::meat::job_capability::JobCapabilities>,
    /// Its `[process_workloads] allowed_binaries`, from its capability
    /// report, when the leader holds one.
    pub allowed_binaries: Option<&'a [std::path::PathBuf]>,
}

/// Why no live node can run `record`, from what the nodes advertise: each
/// distinct reason, quoted as [`unrunnable_reason`] quotes them. `None`
/// while any node might take it (#788).
///
/// Admission only refuses when it is sure: every node must have advertised
/// a summary, and a node with a non-empty allowlist the leader holds no
/// report of counts as allowing the binary. The tick ([`unrunnable_reason`]) stays
/// the backstop for whatever this lets through, and for races such as a
/// node leaving or changing its configuration after admission.
pub fn admission_refusal(record: &TaskArrayRecord, nodes: &[AdvertisedNode<'_>]) -> Option<String> {
    let [first, ..] = nodes else {
        return None;
    };
    // The assignment a node would be sent, so the checks see what it sees.
    let assignment = assignment_for(0, record, &first.node);
    let mut refusals = Vec::with_capacity(nodes.len());
    for node in nodes {
        let jobs = node.jobs?;
        let binary_allowed = node.allowed_binaries.map_or(jobs.binaries, |allowed| {
            allowed.contains(&assignment.program)
        });
        refusals.push(super::task_array_node::capability_refusal(
            &jobs,
            &assignment,
            binary_allowed,
        )?);
    }
    let reasons: BTreeSet<&str> = refusals.iter().map(String::as_str).collect();
    Some(quote_reasons(&reasons))
}

/// One node's answer to a sync call, with the `known` list it was sent.
pub struct SyncAnswer {
    /// The node.
    pub node: NodeId,
    /// The array ids the request listed, which the node has now seen.
    pub known: Vec<u64>,
    /// What it said, or why the call failed.
    pub answer: Result<NodeSyncResponse, String>,
}

/// Sync calls the leader is waiting on. A call that misses one tick's
/// deadline stays here and is collected by a later tick, and its node isn't
/// called again until it answers; [`NODE_SYNC_TIMEOUT`] still bounds it.
#[derive(Default)]
pub struct SyncCalls {
    pending: FuturesUnordered<BoxFuture<'static, SyncAnswer>>,
    waiting_on: HashSet<NodeId>,
}

impl SyncCalls {
    /// Whether a call to `node` is still out.
    pub fn is_waiting_on(&self, node: &NodeId) -> bool {
        self.waiting_on.contains(node)
    }

    /// Track a call to `node`.
    pub fn start(&mut self, node: NodeId, call: BoxFuture<'static, SyncAnswer>) {
        self.waiting_on.insert(node);
        self.pending.push(call);
    }

    /// Every answer that arrives by `deadline`, or sooner if no call is
    /// left. Calls still out stay for the next tick.
    pub async fn gather_until(&mut self, deadline: tokio::time::Instant) -> Vec<SyncAnswer> {
        let mut answers = Vec::new();
        while !self.pending.is_empty() {
            match tokio::time::timeout_at(deadline, self.pending.next()).await {
                Ok(Some(answer)) => {
                    self.waiting_on.remove(&answer.node);
                    answers.push(answer);
                }
                Ok(None) | Err(_) => break,
            }
        }
        answers
    }
}

/// What [`plan_tick`] needs besides the arrays.
pub struct TickInputs<'a> {
    /// The answers that arrived in time, sorted by node.
    pub answers: &'a [(NodeId, NodeSyncResponse)],
    /// When each node last answered.
    pub last_heard: &'a HashMap<NodeId, Instant>,
    /// When this leader started leading.
    pub since: Instant,
    /// Now.
    pub now: Instant,
    /// Silence before a holder's chunks are taken back.
    pub silence: Duration,
    /// The leader's wall clock, for the writes.
    pub now_epoch_secs: u64,
    /// Nodes that could take work: every node the leader syncs whose last
    /// call didn't fail. A run fails as unrunnable only once each of them
    /// has refused it for good.
    pub eligible: BTreeSet<NodeId>,
}

/// Every progress write one tick makes, for every running array, in the
/// order they must apply: a lost holder's `Requeue` (or `Unknown`, for runs
/// that need acknowledged replay) first, then the array's `Sync`, then its
/// quota note. A run no eligible node can run gets one `Unrunnable` write
/// instead (see [`unrunnable_reason`]). [`tick_entries`] commits them as one entry. With `budget`,
/// grants stay within namespace quotas (see [`GrantBudget`]).
pub fn plan_tick(
    arrays: &TaskArrays,
    mut budget: Option<&mut GrantBudget>,
    inputs: &TickInputs<'_>,
) -> Vec<TaskArrayWrite> {
    let mut writes = Vec::new();
    for (batch_id, record) in arrays.active() {
        let for_array: Vec<(NodeId, &ArrayProgress)> = inputs
            .answers
            .iter()
            .filter_map(|(node, response)| {
                response
                    .arrays
                    .iter()
                    .find(|progress| progress.batch_id == batch_id)
                    .map(|progress| (node.clone(), progress))
            })
            .collect();
        if let Some(reason) = unrunnable_reason(record, &for_array, &inputs.eligible) {
            writes.push(TaskArrayWrite::Unrunnable {
                batch_id,
                reason,
                now_epoch_secs: inputs.now_epoch_secs,
            });
            continue;
        }
        let run = arrays.jobs().run(batch_id);
        let replay_unknown = run.is_none_or(|run| run.replay_unknown);
        let mut lost = silent_holders(
            record,
            inputs.last_heard,
            inputs.since,
            inputs.now,
            inputs.silence,
        );
        lost.extend(refusing_holders(record, &for_array));
        for node in lost {
            if replay_unknown {
                writes.push(TaskArrayWrite::Requeue {
                    now_epoch_secs: inputs.now_epoch_secs,
                    batch_id,
                    node,
                });
            } else if run.is_none_or(|run| !run.unknown_owners.contains(&node)) {
                writes.push(TaskArrayWrite::Unknown { batch_id, node });
            }
        }
        let (sync, blocked) = match budget.as_deref_mut() {
            Some(budget) => budget.plan(batch_id, record, &for_array, replay_unknown),
            None => (
                plan_sync(batch_id, record, &for_array, replay_unknown),
                None,
            ),
        };
        if let Some(mut write) = sync {
            write.stamp(inputs.now_epoch_secs);
            writes.push(write);
        }
        writes.extend(quota_blocked_write(batch_id, record, blocked));
    }
    writes
}

/// What the leader loop remembers between ticks.
#[derive(Default)]
struct LeaderMemory {
    /// When this node became leader (`None` while it isn't).
    since: Option<Instant>,
    /// When each node last answered a sync.
    last_heard: HashMap<NodeId, Instant>,
    /// The `known` list each node last acknowledged, so an idle cluster
    /// stops syncing once every node has cleaned up.
    last_known: HashMap<NodeId, Vec<u64>>,
    /// Nodes whose last sync failed, so a dead node is logged once, not
    /// every tick.
    failing: HashSet<NodeId>,
    /// The last cron definition fired, so the next tick starts after it.
    cron_cursor: Option<String>,
    /// Sync calls carried over from earlier ticks.
    calls: SyncCalls,
    /// Which run outcomes this leader has already announced.
    job_events: super::job_events::JobEventTracker,
}

/// Record one event for every run that finished or lost track of its
/// outcome since the last tick, and send the failed and unknown ones to the
/// alert webhook. Delivery runs in the background: a slow receiver must not
/// hold up dispatch.
async fn report_run_changes(state: &ApiState, tracker: &mut super::job_events::JobEventTracker) {
    // The read runs under the control state's lock and must own what it
    // touches, so the tracker goes in and comes back out. If the read
    // fails, the tracker starts afresh and its next look is silent.
    let mut moved = std::mem::take(tracker);
    let Some((returned, changes)) = read_control(state, move |view| {
        let changes = moved.observe(view.arrays);
        (moved, changes)
    })
    .await
    else {
        return;
    };
    *tracker = returned;
    if changes.is_empty() {
        return;
    }
    let now = crate::meat::batch_ids::epoch_now_secs();
    for change in changes {
        let message = change.message();
        let (kind, severity) = change.event_kind();
        if change.notifies() {
            eprintln!("bun: {message}");
        }
        if let Some(events) = &state.events {
            events.write().await.record(
                now,
                kind,
                severity,
                Some(change.name.clone()),
                Some(change.namespace.clone()),
                None,
                message,
            );
        }
        if let (Some(notifier), Some(alert)) = (&state.task_arrays.job_notifier, change.alert()) {
            let notifier = notifier.clone();
            tokio::spawn(async move { notifier.dispatch_job_run(&alert).await });
        }
    }
}

/// Start the leader loop. It runs on every node and does nothing unless
/// the node leads (or is standalone). Stops when the agent stops.
pub(crate) fn spawn_leader_loop(state: ApiState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.task_arrays.sync_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut memory = LeaderMemory::default();
        loop {
            tokio::select! {
                () = state.cmd_tx.closed() => return,
                _ = interval.tick() => {}
            }
            leader_tick(&state, &mut memory).await;
        }
    });
}

async fn is_leading(state: &ApiState) -> bool {
    if state
        .task_arrays
        .storage_fenced
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return false;
    }

    if let Some(store) = &state.task_arrays.standalone {
        let store = store.clone();
        if !tokio::task::spawn_blocking(move || {
            store.lock().map(|store| store.ready()).unwrap_or(false)
        })
        .await
        .unwrap_or(false)
        {
            return false;
        }
    }

    match &state.council {
        Some(council) => council.is_leader().await,
        None => true,
    }
}

/// Every node to sync, with its API URL (`None` for this node).
async fn sync_targets(state: &ApiState) -> Vec<(NodeId, Option<String>)> {
    let self_name = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let mut targets = vec![(NodeId::new(self_name.clone()), None)];
    if let Some(membership) = &state.membership {
        for member in membership.read().await.iter() {
            if member.node_id.0 != self_name {
                let url = state.cluster_http.url(&member.address.to_string(), "");
                targets.push((member.node_id.clone(), Some(url)));
            }
        }
    }
    targets
}

async fn fire_due_schedules(state: &ApiState, cursor: &mut Option<String>) {
    let now = state.task_arrays.cron_clock.now_epoch_secs();
    let Ok(minute) = i64::try_from(now / 60) else {
        return;
    };
    let Ok(at) = time::OffsetDateTime::from_unix_timestamp(i64::try_from(now).unwrap_or(i64::MAX))
    else {
        return;
    };
    let after = cursor.clone();
    let Some((observed, due)) = read_control(state, move |view| {
        let due = due_schedules(view.arrays, at, minute, after.as_deref());
        (view.arrays.cron_observed_minute(), due)
    })
    .await
    else {
        return;
    };
    if observed.is_none_or(|seen| minute > seen) {
        if write_task_array(state, TaskArrayWrite::CronObserve { minute })
            .await
            .is_err()
        {
            return;
        }
    } else if observed.is_some_and(|seen| minute < seen) {
        return;
    }
    let mut fired = Vec::new();
    for schedule in due {
        if !is_leading(state).await {
            return;
        }
        let key = format!("{}/{}", schedule.namespace, schedule.name);
        *cursor = Some(key.clone());
        match write_task_array(
            state,
            TaskArrayWrite::Job(Box::new(crate::meat::job::JobWrite::Fire {
                name: schedule.name.clone(),
                namespace: schedule.namespace.clone(),
                revision: schedule.revision,
                minute,
                now_epoch_secs: now,
            })),
        )
        .await
        {
            Ok(None) => fired.push(schedule),
            Ok(Some(_)) => {}
            Err(error) => eprintln!("bun: cron {key}: {error}"),
        }
    }
    if !fired.is_empty() {
        report_skipped(state, &read_task_arrays(state).await, &fired, minute, now).await;
    }
}

/// Emit one event for every fire this tick that was claimed but skipped
/// because no active-run slot was free. The definition already records it.
async fn report_skipped(
    state: &ApiState,
    arrays: &TaskArrays,
    fired: &[DueSchedule],
    minute: i64,
    now: u64,
) {
    let Some(events) = &state.events else {
        return;
    };
    for schedule in fired {
        let skipped = arrays
            .jobs()
            .definition(&schedule.namespace, &schedule.name)
            .and_then(|record| record.skipped)
            .is_some_and(|skip| skip.minute == minute);
        if !skipped {
            continue;
        }
        eprintln!(
            "bun: cron {}/{}: skipped the {minute} occurrence, no active-run slot was free",
            schedule.namespace, schedule.name
        );
        events.write().await.record(
            now,
            crate::bun::events::EventKind::JobSkipped,
            crate::bun::events::EventSeverity::Warning,
            Some(schedule.name.clone()),
            Some(schedule.namespace.clone()),
            None,
            format!(
                "cron job {}/{} skipped an occurrence: every active-run slot was taken",
                schedule.namespace, schedule.name
            ),
        );
    }
}

/// What a tick reads before it calls the nodes: each node's request, and
/// what the status views keep.
struct SyncRound {
    /// The control version every request carries (the term is filled in
    /// afterwards, from the council).
    version: super::task_array_node::ControlVersion,
    /// Every array id the cluster keeps.
    known: Vec<u64>,
    /// Whether any array is running.
    has_active: bool,
    /// Each node's sync request, with its URL (`None` for this node).
    requests: Vec<(NodeId, Option<String>, NodeSyncRequest)>,
    /// Ids of the arrays and manifests the cluster keeps.
    kept: BTreeSet<u64>,
    /// Ids of the arrays alone.
    kept_arrays: BTreeSet<u64>,
    /// Succeeded and failed counts per array, for rates.
    counts: Vec<(u64, (u64, u64))>,
}

fn sync_round(view: ControlView<'_>, targets: Vec<(NodeId, Option<String>)>) -> SyncRound {
    let arrays = view.arrays;
    let version = super::task_array_node::ControlVersion {
        epoch: view.desired.map_or(0, |desired| desired.recovery_epoch),
        index: view.desired.map_or(view.revision, |desired| {
            desired.last_applied_log.map_or(0, |id| id.index)
        }),
        ..Default::default()
    };
    let kept_arrays: BTreeSet<u64> = arrays.iter().map(|(id, _)| id).collect();
    SyncRound {
        version,
        known: arrays.ids(),
        has_active: arrays.has_active(),
        requests: targets
            .into_iter()
            .map(|(node, url)| {
                let request = sync_request_for(arrays, &node);
                (node, url, request)
            })
            .collect(),
        kept: kept_arrays
            .iter()
            .copied()
            .chain(arrays.manifests().map(|(id, _)| id))
            .collect(),
        counts: arrays
            .iter()
            .map(|(id, record)| {
                let summary = record.state.summary();
                (id, (summary.succeeded, summary.failed))
            })
            .collect(),
        kept_arrays,
    }
}

/// Drop finished runs past retention with one replicated write, so an
/// idle cluster forgets them too. Workers delete a run's files once the
/// leader stops listing it. Nothing is written while nothing is due.
async fn prune_retired_runs(state: &ApiState) {
    let now = crate::meat::batch_ids::epoch_now_secs();
    let retention_secs = state.task_arrays.retention.as_secs();
    let expired = read_control(state, move |view| {
        view.arrays.has_expired(now, retention_secs)
    })
    .await
    .unwrap_or(false);
    if !expired {
        return;
    }
    let write = TaskArrayWrite::Prune {
        now_epoch_secs: now,
        retention_secs,
    };
    if let Err(error) = write_task_array(state, write).await {
        eprintln!("bun: task arrays: retention write failed: {error}");
    }
}

async fn leader_tick(state: &ApiState, memory: &mut LeaderMemory) {
    if !is_leading(state).await {
        *memory = LeaderMemory::default();
        state.task_arrays.rates.lock().await.clear();
        return;
    }
    super::job_apply::settle(state).await;
    fire_due_schedules(state, &mut memory.cron_cursor).await;
    prune_retired_runs(state).await;
    let now = Instant::now();
    let since = *memory.since.get_or_insert(now);
    let targets = sync_targets(state).await;
    let target_nodes: Vec<NodeId> = targets.iter().map(|(node, _)| node.clone()).collect();
    let Some(mut round) = read_control(state, move |view| sync_round(view, targets)).await else {
        return;
    };
    if let Some(council) = &state.council {
        round.version.term = council.current_term();
    }
    if !is_leading(state).await {
        return;
    }
    report_run_changes(state, &mut memory.job_events).await;
    // Even an empty restored snapshot must reach workers: they can hold
    // pre-recovery attempts or directories unknown to this council history.
    // last_known below suppresses repeats only after a successful sync.
    for (node, url, mut request) in std::mem::take(&mut round.requests) {
        if memory.calls.is_waiting_on(&node) {
            continue;
        }
        request.version = round.version;
        if request.arrays.is_empty() && memory.last_known.get(&node) == Some(&round.known) {
            continue;
        }
        let known = round.known.clone();
        let caller = state.clone();
        let call = tokio::spawn(async move { sync_node(&caller, url.as_deref(), &request).await });
        let answering = node.clone();
        memory.calls.start(
            node,
            Box::pin(async move {
                SyncAnswer {
                    node: answering,
                    known,
                    answer: call.await.unwrap_or_else(|error| Err(error.to_string())),
                }
            }),
        );
    }
    let deadline = tokio::time::Instant::now() + sync_deadline(state.task_arrays.sync_interval);
    let mut answers: Vec<(NodeId, NodeSyncResponse)> = Vec::new();
    for SyncAnswer {
        node,
        known,
        answer,
    } in memory.calls.gather_until(deadline).await
    {
        match answer {
            Ok(response) => {
                memory.failing.remove(&node);
                memory.last_heard.insert(node.clone(), Instant::now());
                memory.last_known.insert(node.clone(), known);
                answers.push((node, response));
            }
            Err(reason) => {
                if round.has_active && memory.failing.insert(node.clone()) {
                    eprintln!("bun: task arrays: sync with {node} failed: {reason}");
                }
            }
        }
    }
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    remember_views(state, &answers, &round).await;
    // The latest slots each node reported, this tick or earlier: a node
    // that missed one sync keeps its last word, rather than being charged
    // every task it holds and blocking its namespace for a tick.
    let reported: HashMap<(u64, NodeId), u32> = state
        .task_arrays
        .views
        .lock()
        .await
        .iter()
        .flat_map(|(batch_id, views)| {
            views
                .iter()
                .map(|(node, view)| ((*batch_id, node.clone()), view.slots))
        })
        .collect();
    // A node whose calls fail can't take work, so it doesn't hold a run
    // open; one whose call is merely still out does, until it answers or
    // times out.
    let eligible: BTreeSet<NodeId> = target_nodes
        .into_iter()
        .filter(|node| !memory.failing.contains(node))
        .collect();
    let last_heard = memory.last_heard.clone();
    let silence = state.task_arrays.silence_timeout;
    let now_epoch_secs = crate::meat::batch_ids::epoch_now_secs();
    // Plan under the state's read lock: nothing is copied but each array's
    // state, and only for arrays that finished a chunk.
    let Some(writes) = read_control(state, move |view| {
        let mut budget = view.desired.and_then(|desired| {
            GrantBudget::new(
                &desired.namespaces,
                &desired.scheduling,
                view.arrays,
                |batch_id, node| reported.get(&(batch_id, node.clone())).copied(),
            )
        });
        let inputs = TickInputs {
            answers: &answers,
            last_heard: &last_heard,
            since,
            now,
            silence,
            now_epoch_secs,
            eligible,
        };
        plan_tick(view.arrays, budget.as_mut(), &inputs)
    })
    .await
    else {
        return;
    };
    match write_tick(state, writes).await {
        Ok(refused) => {
            for (batch_id, reason) in refused {
                eprintln!("bun: task array {batch_id}: tick write refused: {reason}");
            }
        }
        Err(error) => eprintln!("bun: task arrays: tick write failed: {error}"),
    }
}

/// Keep each node's latest slots and counters for status views, and drop
/// views of arrays the cluster no longer keeps.
async fn remember_views(
    state: &ApiState,
    answers: &[(NodeId, NodeSyncResponse)],
    round: &SyncRound,
) {
    state
        .task_arrays
        .rates
        .lock()
        .await
        .retain(|id, _| round.kept.contains(id));
    for (id, counts) in &round.counts {
        state.task_arrays.rates(*id, *counts).await;
    }
    let mut views = state.task_arrays.views.lock().await;
    views.retain(|batch_id, _| round.kept_arrays.contains(batch_id));
    for (node, response) in answers {
        for progress in &response.arrays {
            views.entry(progress.batch_id).or_default().insert(
                node.clone(),
                NodeView {
                    node: node.clone(),
                    slots: progress.slots,
                    refused: progress.refused.clone(),
                    counters: progress.counters,
                },
            );
        }
    }
}

/// One node's sync: in process for this node, over HTTP for the others.
async fn sync_node(
    state: &ApiState,
    url: Option<&str>,
    request: &NodeSyncRequest,
) -> Result<NodeSyncResponse, String> {
    let Some(url) = url else {
        return match &state.task_arrays.node {
            Some(node) => node.sync(request).await.map_err(|error| error.to_string()),
            None => Err("this node has no task executor".to_string()),
        };
    };
    let mut call = state
        .cluster_http
        .client()
        .post(format!("{url}/v1/batch/array/sync"))
        .json(request);
    if let Some(token) = &state.service_token {
        call = call.bearer_auth(token);
    }
    let response = tokio::time::timeout(NODE_SYNC_TIMEOUT, call.send())
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("answered {}", response.status()));
    }
    tokio::time::timeout(NODE_SYNC_TIMEOUT, response.json::<NodeSyncResponse>())
        .await
        .map_err(|_| "timed out reading the answer".to_string())?
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::job::JobSpec;
    use crate::meat::index_set::IndexRangeSet;
    use crate::meat::job_capability::JobCapabilities;
    use crate::meat::task_array::TaskArraySpec;
    use crate::meat::task_array_state::{ChunkResult, TaskArrayState};

    #[test]
    fn a_manual_cron_clock_holds_its_time_until_set_and_its_clones_share_it() {
        let clock = CronClock::manual(1_000);
        let shared = clock.clone();
        assert_eq!(clock.now_epoch_secs(), 1_000);
        shared.set(4_600);
        assert_eq!(clock.now_epoch_secs(), 4_600);
    }

    #[test]
    fn the_system_cron_clock_follows_the_wall_clock() {
        let before = crate::meat::batch_ids::epoch_now_secs();
        let now = CronClock::System.now_epoch_secs();
        assert!(now >= before);
        assert!(now <= crate::meat::batch_ids::epoch_now_secs());
    }

    #[test]
    fn setting_the_system_cron_clock_changes_nothing() {
        let clock = CronClock::System;
        clock.set(5);
        assert!(clock.now_epoch_secs() > 5);
    }

    fn record(count: u32, chunk_size: u32) -> TaskArrayRecord {
        let mut env = BTreeMap::new();
        env.insert("PLAIN".to_string(), EnvValue::Plain("yes".to_string()));
        TaskArrayRecord {
            terminal_at_epoch_secs: None,
            quota_blocked: None,
            unrunnable: None,
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: JobSpec {
                pids: None,
                runtime: crate::config::job::JobRuntime::Process,
                image: None,
                command: Some(vec!["frame".to_string(), "{index}".to_string()]),
                schedule: None,
                run_before: Vec::new(),
                memory: None,
                cpu: None,
                env,
                namespace: None,
                exec: Some("/usr/bin/render".into()),
                script: None,
                firewall: None,
                egress: None,
                max_attempts: None,
                task_timeout_secs: None,
                overlap: None,
                replay_unknown: false,
            },
            state: TaskArrayState::new(
                TaskArraySpec {
                    chunk_size,
                    ..TaskArraySpec::with_count(count)
                },
                1,
            )
            .unwrap(),
        }
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn progress(slots: u32, finished: Vec<ChunkResult>) -> ArrayProgress {
        ArrayProgress {
            batch_id: 1,
            slots,
            refused: None,
            unrunnable: false,
            finished,
            counters: NodeArrayCounters::default(),
        }
    }

    fn done(chunk: u32, attempt: u64, succeeded: u32) -> ChunkResult {
        ChunkResult {
            duration_counts: [0; 16],
            chunk: ChunkId(chunk),
            attempt,
            succeeded,
            failed_count: 0,
            failed_indices: IndexRangeSet::new(),
            not_run: 0,
            retried: 0,
            retry_causes: Default::default(),
        }
    }

    #[test]
    fn an_assignment_carries_the_nodes_chunks_at_their_attempts() {
        let mut record = record(40, 10);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        record.state.requeue_node(&node("a"));
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=0))
            .unwrap();
        let assignment = assignment_for(1, &record, &node("a"));
        assert_eq!(
            assignment.held,
            vec![HeldChunk {
                chunk: ChunkId(0),
                attempt: 2
            }]
        );
        assert_eq!(assignment.program.to_str(), Some("/usr/bin/render"));
        assert_eq!(assignment.args, vec!["frame", "{index}"]);
        assert_eq!(
            assignment.env,
            vec![
                ("PLAIN".to_string(), "yes".to_string()),
                ("RELIABURGER_JOB_NAME".to_string(), "render".to_string())
            ]
        );
        assert!(!assignment.stopping);
        assert!(assignment_for(1, &record, &node("b")).held.is_empty());
    }

    #[test]
    fn a_sync_request_lists_every_kept_array_but_assigns_only_running_ones() {
        let mut arrays = TaskArrays::default();
        let register = |count| TaskArrayWrite::Register {
            name: "x".to_string(),
            namespace: "default".to_string(),
            template: Box::new(record(1, 1).template),
            spec: TaskArraySpec::with_count(count),
            submitted_at_epoch_secs: 1,
        };
        arrays.apply(&register(5), || 1).unwrap();
        arrays.apply(&register(5), || 2).unwrap();
        arrays
            .apply(
                &TaskArrayWrite::Cancel {
                    now_epoch_secs: 0,
                    batch_id: 1,
                },
                || 0,
            )
            .unwrap();
        let request = sync_request_for(&arrays, &node("a"));
        assert_eq!(request.known, vec![1, 2]);
        let assigned: Vec<u64> = request.arrays.iter().map(|a| a.batch_id).collect();
        assert_eq!(
            assigned,
            vec![2],
            "a cancelled, drained array isn't assigned"
        );
    }

    #[test]
    fn the_first_sync_grants_by_slots_and_skips_nodes_that_cant_run() {
        let record = record(100, 10);
        let answers = [progress(4, vec![]), progress(0, vec![])];
        let write = plan_sync(
            1,
            &record,
            &[(node("a"), &answers[0]), (node("b"), &answers[1])],
            true,
        )
        .unwrap();
        let TaskArrayWrite::Sync {
            now_epoch_secs: 0,
            results,
            grants,
            ..
        } = write
        else {
            panic!("expected a sync");
        };
        assert!(results.is_empty());
        assert_eq!(grants, vec![(node("a"), IndexRangeSet::from_range(0..=1))]);
    }

    #[test]
    fn a_finished_chunk_is_retired_and_replaced_in_one_entry() {
        let mut record = record(100, 10);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        let answer = progress(4, vec![done(0, 1, 10)]);
        let write = plan_sync(1, &record, &[(node("a"), &answer)], true).unwrap();
        assert_eq!(
            write,
            TaskArrayWrite::Sync {
                now_epoch_secs: 0,
                batch_id: 1,
                results: vec![(node("a"), done(0, 1, 10))],
                grants: vec![(node("a"), IndexRangeSet::from_range(2..=2))],
            }
        );
    }

    #[test]
    fn runs_without_automatic_replay_are_granted_the_small_window() {
        let mut record = record(100_000, 1000);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        let fast = |chunk| {
            let mut result = done(chunk, 1, 1000);
            result.duration_counts[0] = 1000;
            result
        };
        let answer = progress(27, vec![fast(0), fast(1)]);
        let grants =
            |replay_unknown| match plan_sync(1, &record, &[(node("a"), &answer)], replay_unknown) {
                Some(TaskArrayWrite::Sync { grants, .. }) => grants,
                other => panic!("expected a sync, got {other:?}"),
            };
        assert_eq!(
            grants(true),
            vec![(node("a"), IndexRangeSet::from_range(2..=17))]
        );
        assert_eq!(
            grants(false),
            vec![(node("a"), IndexRangeSet::from_range(2..=3))]
        );
    }

    #[test]
    fn nothing_to_say_writes_nothing() {
        let mut record = record(20, 10);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        let answer = progress(4, vec![]);
        assert_eq!(plan_sync(1, &record, &[(node("a"), &answer)], true), None);
    }

    #[test]
    fn stale_and_repeated_reports_are_left_out() {
        let mut record = record(100, 10);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        record.state.complete(&node("a"), &done(0, 1, 10)).unwrap();
        record.state.requeue_node(&node("a"));
        // Chunk 0 is already done; chunk 1 went back to the queue at
        // attempt 2, so "a"'s attempt-1 report is stale.
        let answer = progress(0, vec![done(0, 1, 10), done(1, 1, 10)]);
        assert_eq!(plan_sync(1, &record, &[(node("a"), &answer)], true), None);
    }

    #[test]
    fn a_holder_that_can_no_longer_run_the_array_is_requeued() {
        let mut record = record(100, 10);
        for (name, chunk) in [("a", 0), ("b", 1)] {
            record
                .state
                .grant(&node(name), &IndexRangeSet::from_range(chunk..=chunk))
                .unwrap();
        }
        let mut refusing = progress(0, vec![]);
        refusing.refused = Some("not allowed".to_string());
        let fine = progress(4, vec![]);
        let also_refusing_but_empty = refusing.clone();
        let answers = [
            (node("a"), &refusing),
            (node("b"), &fine),
            (node("c"), &also_refusing_but_empty),
        ];
        assert_eq!(refusing_holders(&record, &answers), vec![node("a")]);
    }

    #[test]
    fn only_holders_that_stayed_quiet_too_long_are_requeued() {
        let mut record = record(100, 10);
        for (name, chunk) in [("a", 0), ("b", 1), ("c", 2)] {
            record
                .state
                .grant(&node(name), &IndexRangeSet::from_range(chunk..=chunk))
                .unwrap();
        }
        let since = Instant::now();
        let now = since + Duration::from_secs(40);
        let mut heard = HashMap::new();
        heard.insert(node("a"), since + Duration::from_secs(35));
        heard.insert(node("b"), since + Duration::from_secs(5));
        // "c" was never heard from: its clock starts when leadership did.
        let silent = silent_holders(&record, &heard, since, now, Duration::from_secs(30));
        assert_eq!(silent, vec![node("b"), node("c")]);
        // A leader that only just took over gives everyone the full grace.
        let fresh = silent_holders(&record, &HashMap::new(), now, now, Duration::from_secs(30));
        assert!(fresh.is_empty());
    }

    /// Twenty every-minute schedules, `job-00` to `job-19`, registered at
    /// minute 2, so all of them are due at minute 3.
    fn twenty_schedules() -> (TaskArrays, time::OffsetDateTime) {
        let mut arrays = TaskArrays::default();
        for i in 0..20 {
            let mut definition = crate::meat::job::JobDefinition::from_spec(record(1, 1).template);
            definition.cron = Some(crate::meat::job::CronPolicy {
                expression: "* * * * *".into(),
                overlap: Default::default(),
                missed: Default::default(),
            });
            arrays
                .apply(
                    &TaskArrayWrite::Job(Box::new(crate::meat::job::JobWrite::Put {
                        name: format!("job-{i:02}"),
                        namespace: "default".into(),
                        definition: Box::new(definition),
                        trigger: None,
                        now_epoch_secs: 120,
                    })),
                    || 0,
                )
                .unwrap();
        }
        let at = time::OffsetDateTime::from_unix_timestamp(180).unwrap();
        (arrays, at)
    }

    fn names(due: &[DueSchedule]) -> Vec<&str> {
        due.iter().map(|due| due.name.as_str()).collect()
    }

    #[test]
    fn due_schedules_take_turns_after_the_last_one_fired() {
        let (arrays, at) = twenty_schedules();
        let first = due_schedules(&arrays, at, 3, None);
        assert_eq!(first.len(), MAX_FIRES_PER_TICK);
        assert_eq!(first[0].name, "job-00");
        let next = due_schedules(&arrays, at, 3, Some("default/job-15"));
        assert_eq!(
            names(&next[..6]),
            ["job-16", "job-17", "job-18", "job-19", "job-00", "job-01"]
        );
        assert!(due_schedules(&arrays, at, 2, None).is_empty());
    }

    fn quota_namespaces(cpu: &str) -> BTreeMap<String, crate::config::NamespaceSpec> {
        let spec: crate::config::NamespaceSpec = toml::from_str(&format!("cpu = '{cpu}'")).unwrap();
        BTreeMap::from([("default".to_string(), spec)])
    }

    #[test]
    fn a_quota_bounds_grants_and_says_why() {
        // 2 CPUs, 1 CPU per task, a node with four slots: two attempts.
        let record = record(100, 1);
        let mut arrays = TaskArrays::default();
        arrays
            .apply(
                &TaskArrayWrite::Register {
                    name: record.name.clone(),
                    namespace: record.namespace.clone(),
                    template: Box::new(record.template.clone()),
                    spec: record.state.spec.clone(),
                    submitted_at_epoch_secs: 1,
                },
                || 1,
            )
            .unwrap();
        let answer = progress(4, vec![]);
        let answers = [(node("a"), &answer)];
        let placements: HashMap<crate::meat::AppId, Vec<crate::meat::types::Placement>> =
            HashMap::new();
        let mut budget =
            GrantBudget::new(&quota_namespaces("2"), &placements, &arrays, |_, _| Some(4)).unwrap();
        let record = arrays.get(1).unwrap();
        let (write, blocked) = budget.plan(1, record, &answers, true);
        let Some(TaskArrayWrite::Sync { grants, .. }) = write else {
            panic!("expected a sync");
        };
        assert_eq!(grants, vec![(node("a"), IndexRangeSet::from_range(0..=1))]);
        assert!(matches!(
            blocked,
            Some(crate::meat::quota::QuotaError::CpuExceeded { limit: 2000, .. })
        ));
        let write = quota_blocked_write(1, record, blocked.clone()).unwrap();
        assert_eq!(
            write,
            TaskArrayWrite::QuotaBlocked {
                batch_id: 1,
                reason: blocked
            }
        );

        // An app placed in the namespace takes the room first.
        let placements = HashMap::from([(
            crate::meat::AppId::new("web", "default"),
            vec![crate::meat::types::Placement {
                node_id: node("a"),
                resources: crate::meat::Resources::new(2000, 0, 0),
                ordinal: 0,
            }],
        )]);
        let mut budget =
            GrantBudget::new(&quota_namespaces("2"), &placements, &arrays, |_, _| Some(4)).unwrap();
        let (write, blocked) = budget.plan(1, record, &answers, true);
        assert_eq!(write, None);
        assert!(blocked.is_some());

        // No quota anywhere: no budget, and nothing is blocked.
        assert!(GrantBudget::new(&BTreeMap::new(), &placements, &arrays, |_, _| None).is_none());
    }

    /// `count` arrays of 100 tasks in 10-task chunks, ids 1 to `count`.
    fn running_arrays(count: u64) -> TaskArrays {
        let mut arrays = TaskArrays::default();
        for id in 1..=count {
            let record = record(100, 10);
            arrays
                .apply(
                    &TaskArrayWrite::Register {
                        name: format!("render-{id}"),
                        namespace: "default".into(),
                        template: Box::new(record.template),
                        spec: record.state.spec,
                        submitted_at_epoch_secs: 1,
                    },
                    || id,
                )
                .unwrap();
        }
        arrays
    }

    /// A node's answer: `slots` free for every running array, and every
    /// chunk it holds finished.
    fn answer_for(arrays: &TaskArrays, node: &NodeId, slots: u32) -> NodeSyncResponse {
        NodeSyncResponse {
            arrays: arrays
                .active()
                .map(|(batch_id, record)| ArrayProgress {
                    batch_id,
                    slots,
                    refused: None,
                    unrunnable: false,
                    finished: record
                        .state
                        .held_by(node)
                        .into_iter()
                        .flat_map(|held| held.iter())
                        .map(|chunk| done(chunk, 1, 10))
                        .collect(),
                    counters: NodeArrayCounters::default(),
                })
                .collect(),
        }
    }

    fn inputs<'a>(
        answers: &'a [(NodeId, NodeSyncResponse)],
        heard: &'a HashMap<NodeId, Instant>,
    ) -> TickInputs<'a> {
        let now = Instant::now();
        TickInputs {
            answers,
            last_heard: heard,
            since: now,
            now,
            silence: SILENCE_TIMEOUT,
            now_epoch_secs: 7,
            eligible: answers.iter().map(|(node, _)| node.clone()).collect(),
        }
    }

    /// A node's answer refusing every running array for `reason`;
    /// `for_good` says whether the refusal is the node's configuration.
    fn refusal_for(arrays: &TaskArrays, reason: &str, for_good: bool) -> NodeSyncResponse {
        NodeSyncResponse {
            arrays: arrays
                .active()
                .map(|(batch_id, _)| ArrayProgress {
                    unrunnable: for_good,
                    ..refused_progress(batch_id, reason)
                })
                .collect(),
        }
    }

    fn refused_progress(batch_id: u64, reason: &str) -> ArrayProgress {
        ArrayProgress {
            batch_id,
            slots: 0,
            refused: Some(reason.to_string()),
            unrunnable: false,
            finished: Vec::new(),
            counters: NodeArrayCounters::default(),
        }
    }

    const ISOLATION: &str = "task arrays don't run under [process_workloads] mount_isolation yet";

    #[test]
    fn a_run_every_eligible_node_refuses_for_good_fails_with_the_reason() {
        let mut arrays = running_arrays(1);
        let answers = [
            (node("a"), refusal_for(&arrays, ISOLATION, true)),
            (node("b"), refusal_for(&arrays, ISOLATION, true)),
        ];
        let heard = HashMap::new();
        let writes = plan_tick(&arrays, None, &inputs(&answers, &heard));
        let [
            TaskArrayWrite::Unrunnable {
                batch_id: 1,
                reason,
                now_epoch_secs: 7,
            },
        ] = &writes[..]
        else {
            panic!("expected one unrunnable write, got {writes:?}");
        };
        assert!(reason.starts_with("no node can run it"), "{reason}");
        assert_eq!(reason.matches(ISOLATION).count(), 1, "{reason}");
        for entry in tick_entries(writes) {
            arrays.apply(&entry, || 0).unwrap();
        }
        let record = arrays.get(1).unwrap();
        assert_eq!(
            record.state.status(),
            crate::meat::task_array_state::TaskArrayStatus::Failed
        );
        assert!(record.unrunnable.as_deref().unwrap().contains(ISOLATION));
    }

    #[test]
    fn a_run_waits_while_any_eligible_node_could_still_take_it() {
        let arrays = running_arrays(1);
        let heard = HashMap::new();
        let unrunnable = |writes: &[TaskArrayWrite]| {
            writes
                .iter()
                .any(|write| matches!(write, TaskArrayWrite::Unrunnable { .. }))
        };
        // One node refuses, another offers slots.
        let answers = [
            (node("a"), refusal_for(&arrays, ISOLATION, true)),
            (node("b"), answer_for(&arrays, &node("b"), 4)),
        ];
        assert!(!unrunnable(&plan_tick(
            &arrays,
            None,
            &inputs(&answers, &heard)
        )));
        // One node refuses, another hasn't answered yet.
        let answers = [(node("a"), refusal_for(&arrays, ISOLATION, true))];
        let mut tick = inputs(&answers, &heard);
        tick.eligible.insert(node("slow"));
        assert!(!unrunnable(&plan_tick(&arrays, None, &tick)));
        // A passing fault, not the node's configuration.
        let answers = [(
            node("a"),
            refusal_for(&arrays, "can't open the task ledger", false),
        )];
        assert!(!unrunnable(&plan_tick(
            &arrays,
            None,
            &inputs(&answers, &heard)
        )));
        // Nobody answered at all.
        assert!(!unrunnable(&plan_tick(&arrays, None, &inputs(&[], &heard))));
    }

    /// A node that runs every kind of job, with `/usr/bin/render` among
    /// others on its allowlist.
    fn capable() -> JobCapabilities {
        JobCapabilities {
            host: true,
            host_limits: true,
            containers: true,
            singleton_images: true,
            singleton_image_limits: true,
            encrypted_env: true,
            network_policy: true,
            mount_isolation: false,
            binaries: true,
        }
    }

    fn isolated() -> JobCapabilities {
        JobCapabilities {
            mount_isolation: true,
            ..capable()
        }
    }

    fn advertised(name: &str, jobs: Option<JobCapabilities>) -> AdvertisedNode<'static> {
        AdvertisedNode {
            node: node(name),
            jobs,
            allowed_binaries: None,
        }
    }

    /// #788: when every live node advertises that it can't take a run, the
    /// leader refuses it at submission, naming each distinct reason once.
    #[test]
    fn admission_refuses_a_run_every_live_node_advertises_it_cannot_take() {
        let record = record(20, 10);
        let no_host = JobCapabilities {
            host: false,
            ..capable()
        };
        let nodes = [
            advertised("a", Some(isolated())),
            advertised("b", Some(isolated())),
            advertised("c", Some(no_host)),
        ];
        let reason = admission_refusal(&record, &nodes).expect("no node can take it");
        assert_eq!(
            reason.matches("host jobs don't run under").count(),
            1,
            "{reason}"
        );
        assert!(reason.contains("host arrays require"), "{reason}");
    }

    /// #788: admission and the tick backstop quote the same reasons, so a
    /// user sees the same words whichever of them refuses the run.
    #[test]
    fn admission_quotes_the_reasons_the_tick_would() {
        let arrays = running_arrays(1);
        let running = arrays.get(1).unwrap();
        let assignment = assignment_for(1, running, &node("a"));
        let refused =
            crate::bun::task_array_node::capability_refusal(&isolated(), &assignment, true)
                .expect("an isolated node refuses host work");
        let answers = [
            (node("a"), refusal_for(&arrays, &refused, true)),
            (node("b"), refusal_for(&arrays, &refused, true)),
        ];
        let heard = HashMap::new();
        let writes = plan_tick(&arrays, None, &inputs(&answers, &heard));
        let [TaskArrayWrite::Unrunnable { reason: ticked, .. }] = &writes[..] else {
            panic!("expected one unrunnable write, got {writes:?}");
        };
        let admitted = admission_refusal(
            running,
            &[
                advertised("a", Some(isolated())),
                advertised("b", Some(isolated())),
            ],
        )
        .expect("both nodes refuse it");
        assert_eq!(*ticked, format!("no node can run it: {admitted}"));
    }

    /// #788: admission only refuses when it is sure. A node that might take
    /// the run, or one whose capabilities the leader hasn't heard yet,
    /// leaves the decision to the nodes and the tick.
    #[test]
    fn admission_admits_a_run_any_live_node_might_take() {
        let record = record(20, 10);
        assert_eq!(
            admission_refusal(
                &record,
                &[
                    advertised("a", Some(isolated())),
                    advertised("b", Some(capable()))
                ]
            ),
            None
        );
        assert_eq!(
            admission_refusal(
                &record,
                &[advertised("a", Some(isolated())), advertised("b", None)]
            ),
            None,
            "a node that hasn't said might take it"
        );
        assert_eq!(admission_refusal(&record, &[]), None, "no members known");
    }

    /// #788: a container run nobody can start is refused too, not only
    /// host work.
    #[test]
    fn admission_refuses_containers_where_no_node_runs_them() {
        let mut record = record(100, 10);
        record.template.runtime = crate::config::job::JobRuntime::Runc;
        record.template.exec = None;
        record.template.image = Some("fixture:v1".into());
        let process_only = JobCapabilities {
            containers: false,
            singleton_images: false,
            singleton_image_limits: false,
            ..capable()
        };
        let reason = admission_refusal(&record, &[advertised("a", Some(process_only))]).unwrap();
        assert!(reason.contains("container tasks require"), "{reason}");
        assert_eq!(
            admission_refusal(&record, &[advertised("a", Some(isolated()))]),
            None,
            "mount isolation doesn't stop containers"
        );
    }

    /// #741: a pids limit is a limit like CPU and memory, so a host run that
    /// sets one needs a node that enforces host limits.
    #[test]
    fn admission_refuses_a_pids_limit_where_no_node_enforces_host_limits() {
        let mut record = record(20, 10);
        let unlimited = JobCapabilities {
            host_limits: false,
            ..capable()
        };
        assert_eq!(
            admission_refusal(&record, &[advertised("a", Some(unlimited))]),
            None,
            "without a limit the fallback backend may take it"
        );
        record.template.pids = Some(crate::config::types::PidsLimit::try_from(64).unwrap());
        let reason = admission_refusal(&record, &[advertised("a", Some(unlimited))]).unwrap();
        assert!(reason.contains("pids"), "{reason}");
        assert_eq!(
            admission_refusal(&record, &[advertised("a", Some(capable()))]),
            None
        );
    }

    /// #788: the gossip summary only says whether a node allows any binary.
    /// Where the leader holds the node's capability report, it checks the
    /// run's binary against the full allowlist; without it, a node with a
    /// non-empty allowlist might allow the binary.
    #[test]
    fn admission_checks_the_binary_against_a_reported_allowlist() {
        let record = record(20, 10);
        let elsewhere = [std::path::PathBuf::from("/usr/bin/true")];
        let reported = AdvertisedNode {
            allowed_binaries: Some(&elsewhere),
            ..advertised("a", Some(capable()))
        };
        let reason = admission_refusal(&record, &[reported]).unwrap();
        assert!(
            reason.contains(
                "/usr/bin/render isn't in this node's [process_workloads] allowed_binaries"
            ),
            "{reason}"
        );
        assert_eq!(
            admission_refusal(&record, &[advertised("a", Some(capable()))]),
            None
        );
        let empty = JobCapabilities {
            binaries: false,
            ..capable()
        };
        let reason = admission_refusal(&record, &[advertised("a", Some(empty))]).unwrap();
        assert!(reason.contains("allowed_binaries"), "{reason}");
    }

    #[test]
    fn a_refusing_holder_is_requeued_before_its_run_is_failed() {
        let mut arrays = running_arrays(1);
        let heard = HashMap::new();
        let first = [(node("a"), answer_for(&arrays, &node("a"), 10))];
        for entry in tick_entries(plan_tick(&arrays, None, &inputs(&first, &heard))) {
            arrays.apply(&entry, || 0).unwrap();
        }
        // "a" now holds chunks, then its allowlist changes.
        let answers = [(node("a"), refusal_for(&arrays, ISOLATION, true))];
        let writes = plan_tick(&arrays, None, &inputs(&answers, &heard));
        assert!(
            matches!(&writes[..], [TaskArrayWrite::Requeue { .. }, ..]),
            "{writes:?}"
        );
        assert!(
            !writes
                .iter()
                .any(|write| matches!(write, TaskArrayWrite::Unrunnable { .. }))
        );
        for entry in tick_entries(writes) {
            arrays.apply(&entry, || 0).unwrap();
        }
        let writes = plan_tick(&arrays, None, &inputs(&answers, &heard));
        assert!(
            matches!(&writes[..], [TaskArrayWrite::Unrunnable { .. }]),
            "{writes:?}"
        );
    }

    #[test]
    fn tick_writes_one_raft_entry_for_many_arrays() {
        let mut arrays = running_arrays(64);
        let nodes = [node("a"), node("b"), node("c")];
        let heard = HashMap::new();
        let mut entries = 0;
        for _ in 0..2 {
            let answers: Vec<(NodeId, NodeSyncResponse)> = nodes
                .iter()
                .map(|n| (n.clone(), answer_for(&arrays, n, 10)))
                .collect();
            let writes = plan_tick(&arrays, None, &inputs(&answers, &heard));
            assert_eq!(writes.len(), 64, "one sync per array");
            assert!(writes.iter().all(|write| matches!(
                write,
                TaskArrayWrite::Sync {
                    now_epoch_secs: 7,
                    ..
                }
            )));
            let tick = tick_entries(writes);
            assert_eq!(tick.len(), 1, "one entry for every array");
            for entry in tick {
                entries += 1;
                let applied = arrays.apply(&entry, || 0).unwrap();
                assert_eq!(
                    applied,
                    TaskArrayApplied::Ticked {
                        refused: Vec::new()
                    }
                );
            }
        }
        assert_eq!(entries, 2, "two ticks, two entries");
        for (batch_id, record) in arrays.iter() {
            let summary = record.state.summary();
            // The second tick retired the first one's six chunks and handed
            // out the last four.
            assert_eq!(summary.succeeded, 60, "array {batch_id}");
            assert_eq!(summary.held, 40, "array {batch_id}");
        }
        // More writes than one entry carries go out in as few as fit.
        let many = vec![
            TaskArrayWrite::QuotaBlocked {
                batch_id: 1,
                reason: None
            };
            MAX_TICK_WRITES + 1
        ];
        assert_eq!(tick_entries(many).len(), 2);
        assert!(tick_entries(Vec::new()).is_empty());
    }

    #[test]
    fn a_tick_requeues_silent_holders_before_syncing_their_array() {
        let mut arrays = running_arrays(1);
        let first = [(node("a"), answer_for(&arrays, &node("a"), 10))];
        let heard = HashMap::new();
        for entry in tick_entries(plan_tick(&arrays, None, &inputs(&first, &heard))) {
            arrays.apply(&entry, || 0).unwrap();
        }
        // "a" went quiet for longer than the silence timeout; "b" answers.
        let answers = [(node("b"), answer_for(&arrays, &node("b"), 10))];
        let mut tick = inputs(&answers, &heard);
        tick.now = tick.since + SILENCE_TIMEOUT + Duration::from_secs(1);
        let writes = plan_tick(&arrays, None, &tick);
        assert!(matches!(
            &writes[..],
            [
                TaskArrayWrite::Requeue { node: lost, now_epoch_secs: 7, .. },
                TaskArrayWrite::Sync { now_epoch_secs: 7, .. },
            ] if *lost == node("a")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_node_does_not_delay_grants_to_other_nodes() {
        let arrays = running_arrays(1);
        let interval = SYNC_INTERVAL;
        let mut calls = SyncCalls::default();
        let call = |name: &str, delay: Duration| -> BoxFuture<'static, SyncAnswer> {
            let node = node(name);
            let response = answer_for(&arrays, &node, 10);
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                SyncAnswer {
                    node,
                    known: vec![1],
                    answer: Ok(response),
                }
            })
        };
        calls.start(node("fast"), call("fast", Duration::from_millis(20)));
        // A grey failure: gossip still lists it, its API hangs.
        calls.start(node("hung"), call("hung", Duration::from_millis(4200)));

        let started = tokio::time::Instant::now();
        let answers = calls.gather_until(started + sync_deadline(interval)).await;
        assert_eq!(started.elapsed(), sync_deadline(interval));
        assert!(
            started.elapsed() < interval,
            "the tick ends inside its slot"
        );
        let answered: Vec<(NodeId, NodeSyncResponse)> = answers
            .into_iter()
            .map(|answer| (answer.node, answer.answer.unwrap()))
            .collect();
        assert_eq!(answered.len(), 1);
        let heard = HashMap::new();
        let writes = plan_tick(&arrays, None, &inputs(&answered, &heard));
        let [TaskArrayWrite::Sync { grants, .. }] = &writes[..] else {
            panic!("expected one sync, got {writes:?}");
        };
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].0, node("fast"));

        // The next tick doesn't call the hung node again: its call carries
        // over, and the tick that sees it finish collects the answer.
        assert!(calls.is_waiting_on(&node("hung")));
        assert!(!calls.is_waiting_on(&node("fast")));
        let mut ticks = 1;
        let mut late = Vec::new();
        while late.is_empty() {
            ticks += 1;
            let tick = tokio::time::Instant::now();
            late = calls.gather_until(tick + sync_deadline(interval)).await;
            tokio::time::sleep_until(tick + interval).await;
        }
        assert_eq!(late[0].node, node("hung"));
        assert!(!calls.is_waiting_on(&node("hung")));
        assert!(ticks >= 5, "the hung call took {ticks} ticks to answer");
    }

    #[test]
    fn an_unchanged_quota_limit_writes_nothing_new() {
        let mut record = record(10, 1);
        let reason = |current| crate::meat::quota::QuotaError::CpuExceeded {
            namespace: "default".into(),
            current,
            requested: 1000,
            limit: 2000,
        };
        assert_eq!(quota_blocked_write(1, &record, None), None);
        record.quota_blocked = Some(reason(2000));
        assert_eq!(quota_blocked_write(1, &record, Some(reason(1500))), None);
        assert_eq!(
            quota_blocked_write(1, &record, None),
            Some(TaskArrayWrite::QuotaBlocked {
                batch_id: 1,
                reason: None
            })
        );
    }
}

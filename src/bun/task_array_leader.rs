//! The leader's side of task arrays: one loop that talks to every node
//! once a second and turns what they say into at most one Raft entry per
//! array.
//!
//! Each tick the leader reads the replicated [`TaskArrays`], sends every
//! node its share (the chunks it holds, at their attempts) and collects
//! finished chunks and free slots. For each running array it then writes a
//! single `Sync` entry: the finished chunks, and new grants planned with
//! [`plan_grants`] against the state as it will be once those chunks are
//! retired. A node that holds chunks but hasn't answered for
//! [`SILENCE_TIMEOUT`] gets a `Requeue`, which hands its chunks to others
//! at the next attempt, so a late report from it is fenced off.
//!
//! Production persists the same state through a council or the private
//! standalone job store. Tests may explicitly select volatile standalone state.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    TaskArrayApplied, TaskArrayRecord, TaskArrayWrite, TaskArrays,
};

/// How often the leader syncs every node.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// How long a node holding chunks may go without answering before the
/// leader takes them back.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one node's sync call may take.
pub const NODE_SYNC_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a task-array Raft write may take before the caller gives up
/// (the entry may still commit; every write is safe to repeat or drop).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Time between syncs.
    pub sync_interval: Duration,
    /// Silence before a node's chunks are taken back.
    pub silence_timeout: Duration,
    /// Standalone copy of the arrays (unused with a council).
    local: Mutex<TaskArrays>,
    /// What each node last said about each array, by batch id.
    views: Mutex<HashMap<u64, BTreeMap<NodeId, NodeView>>>,
    rates: Mutex<HashMap<u64, super::task_rates::RateSample>>,
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
            node,
            sync_interval,
            silence_timeout,
            local: Mutex::new(TaskArrays::default()),
            views: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
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

/// Commit a write: through Raft with a council, in memory standalone.
/// Returns the new id for a registration.
pub(crate) async fn write_task_array(
    state: &ApiState,
    mut write: TaskArrayWrite,
) -> Result<Option<u64>, TaskArrayWriteError> {
    match &mut write {
        TaskArrayWrite::Sync { now_epoch_secs, .. }
        | TaskArrayWrite::Cancel { now_epoch_secs, .. }
        | TaskArrayWrite::Requeue { now_epoch_secs, .. }
        | TaskArrayWrite::Replay { now_epoch_secs, .. }
        | TaskArrayWrite::CancelManifest { now_epoch_secs, .. }
            if *now_epoch_secs == 0 =>
        {
            *now_epoch_secs = crate::meat::batch_tracker::epoch_now_secs();
        }
        _ => {}
    }
    let Some(council) = &state.council else {
        if state.task_arrays.require_council {
            return Err(TaskArrayWriteError::Unavailable(
                "delegated arrays require a council for durable definitions and identities".into(),
            ));
        }
        if let Some(store) = &state.task_arrays.standalone {
            if state
                .task_arrays
                .storage_fenced
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(TaskArrayWriteError::Unavailable(
                    "standalone publication timed out; restart and reconcile".into(),
                ));
            }
            let store = Arc::clone(store);
            let publication = tokio::task::spawn_blocking(move || {
                let mut store = store.lock().map_err(|_| {
                    TaskArrayWriteError::Unavailable("job store lock poisoned".into())
                })?;
                store.apply(&write).map_err(|error| {
                    if store.ready() {
                        TaskArrayWriteError::Refused(error)
                    } else {
                        TaskArrayWriteError::Unavailable(error)
                    }
                })
            });
            return match tokio::time::timeout(WRITE_TIMEOUT, publication).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    state
                        .task_arrays
                        .storage_fenced
                        .store(true, std::sync::atomic::Ordering::Release);
                    Err(TaskArrayWriteError::Unavailable(error.to_string()))
                }
                Err(_) => {
                    state
                        .task_arrays
                        .storage_fenced
                        .store(true, std::sync::atomic::Ordering::Release);
                    Err(TaskArrayWriteError::Unavailable(
                        "standalone publication timed out; admission and dispatch are fenced"
                            .into(),
                    ))
                }
            };
        }
        let mut tracker = state.batch_tracker.lock().await;
        let mut arrays = state.task_arrays.local.lock().await;
        let ids = arrays
            .planned_ids(&write)
            .map_err(|error| TaskArrayWriteError::Refused(error.to_string()))?;
        tracker
            .preflight_ids(ids)
            .map_err(TaskArrayWriteError::Refused)?;
        return match arrays.apply(&write, || tracker.allocate_id()) {
            Ok(TaskArrayApplied::Registered { batch_id }) => Ok(Some(batch_id)),
            Ok(_) => Ok(None),
            Err(error) => Err(TaskArrayWriteError::Refused(error.to_string())),
        };
    };
    let written = tokio::time::timeout(
        WRITE_TIMEOUT,
        council.write(RaftRequest::TaskArray(Box::new(write))),
    )
    .await
    .map_err(|_| TaskArrayWriteError::Unavailable("the Raft write timed out".to_string()))?;
    match written {
        Ok(CouncilResponse::TaskArrayRegistered { batch_id }) => Ok(Some(batch_id)),
        Ok(CouncilResponse::Refused { reason }) => Err(TaskArrayWriteError::Refused(reason)),
        Ok(_) => Ok(None),
        Err(error) => Err(TaskArrayWriteError::Unavailable(error.to_string())),
    }
}

/// The current arrays: the replicated copy with a council, the local one
/// standalone.
pub(crate) async fn read_task_arrays(state: &ApiState) -> TaskArrays {
    match &state.council {
        Some(council) => council.desired_state().await.task_arrays,
        None => local_snapshot(&state.task_arrays).await.0,
    }
}

async fn local_snapshot(service: &TaskArrayService) -> (TaskArrays, u64) {
    if let Some(store) = &service.standalone {
        let store = store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (store.arrays().clone(), store.revision())
        })
        .await
        .unwrap_or_else(|_| (TaskArrays::default(), 0))
    } else {
        (service.local.lock().await.clone(), 0)
    }
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
    }
}

/// The whole sync call for one node: every running array, plus the ids of
/// every array the cluster still keeps.
pub fn sync_request_for(arrays: &TaskArrays, node: &NodeId) -> NodeSyncRequest {
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
    let mut preview = record.state.clone();
    let mut results = Vec::new();
    for (node, progress) in answers {
        for result in &progress.finished {
            if preview.complete(node, result).is_ok()
                && !record.state.done().contains(result.chunk.0)
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
/// for it yet, at most [`MAX_FIRES_PER_TICK`]. Schedules a pending
/// deployment `blocked` are left out *before* the limit, so they can't use
/// up the places. The list starts after `cursor` (the last key fired) and
/// wraps round, so a long list takes turns instead of starving its tail.
pub fn due_schedules(
    arrays: &TaskArrays,
    at: time::OffsetDateTime,
    minute: i64,
    blocked: impl Fn(&str, &str) -> bool,
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
            (!blocked(name, namespace)).then(|| DueSchedule {
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

/// What the leader loop remembers between ticks.
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
    failing: std::collections::HashSet<NodeId>,
    /// The last cron definition fired, so the next tick starts after it.
    cron_cursor: Option<String>,
}

/// Start the leader loop. It runs on every node and does nothing unless
/// the node leads (or is standalone). Stops when the agent stops.
pub(crate) fn spawn_leader_loop(state: ApiState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.task_arrays.sync_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut memory = LeaderMemory {
            since: None,
            last_heard: HashMap::new(),
            last_known: HashMap::new(),
            failing: std::collections::HashSet::new(),
            cron_cursor: None,
        };
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
    let now = crate::meat::batch_tracker::epoch_now_secs();
    let Ok(minute) = i64::try_from(now / 60) else {
        return;
    };
    let arrays = read_task_arrays(state).await;
    if arrays
        .cron_observed_minute()
        .is_none_or(|seen| minute > seen)
    {
        if write_task_array(state, TaskArrayWrite::CronObserve { minute })
            .await
            .is_err()
        {
            return;
        }
    } else if arrays
        .cron_observed_minute()
        .is_some_and(|seen| minute < seen)
    {
        return;
    }
    let Ok(at) = time::OffsetDateTime::from_unix_timestamp(i64::try_from(now).unwrap_or(i64::MAX))
    else {
        return;
    };
    let claims = match &state.council {
        Some(council) => council.desired_state().await.prerequisite_claims,
        None => Default::default(),
    };
    let due = due_schedules(
        &arrays,
        at,
        minute,
        |name, namespace| claims.values().any(|claim| claim.blocks(name, namespace)),
        cursor.as_deref(),
    );
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

async fn leader_tick(state: &ApiState, memory: &mut LeaderMemory) {
    if !is_leading(state).await {
        memory.since = None;
        memory.last_heard.clear();
        memory.last_known.clear();
        memory.failing.clear();
        memory.cron_cursor = None;
        state.task_arrays.rates.lock().await.clear();
        return;
    }
    super::job_apply::settle(state).await;
    fire_due_schedules(state, &mut memory.cron_cursor).await;
    let now = Instant::now();
    let since = *memory.since.get_or_insert(now);
    let (arrays, version, quotas) = match &state.council {
        Some(council) => {
            let desired = council.desired_state().await;
            let version = super::task_array_node::ControlVersion {
                epoch: desired.recovery_epoch,
                term: council.current_term(),
                index: desired.last_applied_log.map_or(0, |id| id.index),
            };
            let quotas = (desired.namespaces, desired.scheduling);
            (desired.task_arrays, version, Some(quotas))
        }
        None => {
            let (arrays, revision) = local_snapshot(&state.task_arrays).await;
            (
                arrays,
                super::task_array_node::ControlVersion {
                    index: revision,
                    ..Default::default()
                },
                None,
            )
        }
    };
    if !is_leading(state).await {
        return;
    }
    let known = arrays.ids();
    // Even an empty restored snapshot must reach workers: they can hold
    // pre-recovery attempts or directories unknown to this council history.
    // last_known below suppresses repeats only after a successful sync.

    let mut calls = tokio::task::JoinSet::new();
    for (node, url) in sync_targets(state).await {
        let mut request = sync_request_for(&arrays, &node);
        request.version = version;
        if request.arrays.is_empty() && memory.last_known.get(&node) == Some(&known) {
            continue;
        }
        let state = state.clone();
        calls.spawn(async move {
            let answer = sync_node(&state, url.as_deref(), &request).await;
            (node, answer)
        });
    }
    let mut answers: Vec<(NodeId, NodeSyncResponse)> = Vec::new();
    while let Some(joined) = calls.join_next().await {
        let Ok((node, answer)) = joined else { continue };
        match answer {
            Ok(response) => {
                memory.failing.remove(&node);
                memory.last_heard.insert(node.clone(), Instant::now());
                memory.last_known.insert(node.clone(), known.clone());
                answers.push((node, response));
            }
            Err(reason) => {
                if arrays.has_active() && memory.failing.insert(node.clone()) {
                    eprintln!("bun: task arrays: sync with {node} failed: {reason}");
                }
            }
        }
    }
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    remember_views(state, &answers, &arrays).await;
    let mut budget = quotas.and_then(|(namespaces, scheduling)| {
        GrantBudget::new(&namespaces, &scheduling, &arrays, |batch_id, node| {
            answers
                .iter()
                .find(|(answered, _)| answered == node)?
                .1
                .arrays
                .iter()
                .find(|progress| progress.batch_id == batch_id)
                .map(|progress| progress.slots)
        })
    });

    for (batch_id, record) in arrays.active() {
        let for_array: Vec<(NodeId, &ArrayProgress)> = answers
            .iter()
            .filter_map(|(node, response)| {
                response
                    .arrays
                    .iter()
                    .find(|progress| progress.batch_id == batch_id)
                    .map(|progress| (node.clone(), progress))
            })
            .collect();
        let mut lost = silent_holders(
            record,
            &memory.last_heard,
            since,
            now,
            state.task_arrays.silence_timeout,
        );
        lost.extend(refusing_holders(record, &for_array));
        for node in lost {
            let write = if arrays
                .jobs()
                .run(batch_id)
                .is_some_and(|run| !run.replay_unknown)
            {
                if arrays
                    .jobs()
                    .run(batch_id)
                    .is_some_and(|run| run.unknown_owners.contains(&node))
                {
                    continue;
                }
                TaskArrayWrite::Unknown { batch_id, node }
            } else {
                TaskArrayWrite::Requeue {
                    now_epoch_secs: crate::meat::batch_tracker::epoch_now_secs(),
                    batch_id,
                    node,
                }
            };
            if let Err(error) = write_task_array(state, write).await {
                eprintln!("bun: task array {batch_id}: owner-loss write failed: {error}");
            }
        }
        let replay_unknown = arrays
            .jobs()
            .run(batch_id)
            .is_none_or(|run| run.replay_unknown);
        let (sync, blocked) = match budget.as_mut() {
            Some(budget) => budget.plan(batch_id, record, &for_array, replay_unknown),
            None => (
                plan_sync(batch_id, record, &for_array, replay_unknown),
                None,
            ),
        };
        if let Some(write) = sync
            && let Err(error) = write_task_array(state, write).await
        {
            eprintln!("bun: task array {batch_id}: sync write failed: {error}");
        }
        if let Some(write) = quota_blocked_write(batch_id, record, blocked)
            && let Err(error) = write_task_array(state, write).await
        {
            eprintln!("bun: task array {batch_id}: quota reason write failed: {error}");
        }
    }
}

/// Keep each node's latest slots and counters for status views, and drop
/// views of arrays the cluster no longer keeps.
async fn remember_views(
    state: &ApiState,
    answers: &[(NodeId, NodeSyncResponse)],
    arrays: &TaskArrays,
) {
    state
        .task_arrays
        .rates
        .lock()
        .await
        .retain(|id, _| arrays.get(*id).is_some() || arrays.manifest(*id).is_some());
    for (id, record) in arrays.iter() {
        let summary = record.state.summary();
        state
            .task_arrays
            .rates(id, (summary.succeeded, summary.failed))
            .await;
    }
    let mut views = state.task_arrays.views.lock().await;
    views.retain(|batch_id, _| arrays.get(*batch_id).is_some());
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
            Some(node) => Ok(node.sync(request).await),
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
    use crate::meat::task_array::TaskArraySpec;
    use crate::meat::task_array_state::{ChunkResult, TaskArrayState};

    fn record(count: u32, chunk_size: u32) -> TaskArrayRecord {
        let mut env = BTreeMap::new();
        env.insert("PLAIN".to_string(), EnvValue::Plain("yes".to_string()));
        TaskArrayRecord {
            terminal_at_epoch_secs: None,
            quota_blocked: None,
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: JobSpec {
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
    fn blocked_schedules_do_not_starve_later_ones() {
        let (arrays, at) = twenty_schedules();
        // A pending deployment blocks the first sixteen. Picking sixteen
        // before checking would leave nothing to fire all minute.
        let blocked = |name: &str, _: &str| name < "job-16";
        let due = due_schedules(&arrays, at, 3, blocked, None);
        assert_eq!(names(&due), ["job-16", "job-17", "job-18", "job-19"]);
    }

    #[test]
    fn due_schedules_take_turns_after_the_last_one_fired() {
        let (arrays, at) = twenty_schedules();
        let first = due_schedules(&arrays, at, 3, |_, _| false, None);
        assert_eq!(first.len(), MAX_FIRES_PER_TICK);
        assert_eq!(first[0].name, "job-00");
        let next = due_schedules(&arrays, at, 3, |_, _| false, Some("default/job-15"));
        assert_eq!(
            names(&next[..6]),
            ["job-16", "job-17", "job-18", "job-19", "job-00", "job-01"]
        );
        assert!(due_schedules(&arrays, at, 2, |_, _| false, None).is_empty());
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

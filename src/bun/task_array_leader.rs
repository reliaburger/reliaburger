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
//! Tests can run the same loop with volatile standalone state. Production
//! admission requires a council for durable definitions and identities.

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
use crate::meat::task_array_state::{NodeSlots, plan_grants};
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
    /// Image signature policy applied before admission and dispatch.
    pub trust_policy: crate::config::node::TrustPolicySection,
    /// This node's executor; `None` means this node can't run tasks
    /// (it still coordinates when it leads).
    pub node: Option<Arc<TaskArrayNode>>,
    /// Production definitions require replicated storage, never volatile IDs.
    require_council: bool,
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

    /// Apply the configured image policy to batch admission too.
    pub fn with_trust_policy(mut self, policy: crate::config::node::TrustPolicySection) -> Self {
        self.trust_policy = policy;
        self
    }

    /// Test harness service with explicit timings and volatile standalone state.
    /// Production uses `new`, which requires a council for durable definitions.
    pub fn with_timings(
        node: Option<Arc<TaskArrayNode>>,
        sync_interval: Duration,
        silence_timeout: Duration,
    ) -> Self {
        Self {
            trust_policy: Default::default(),
            require_council: false,
            node,
            sync_interval,
            silence_timeout,
            local: Mutex::new(TaskArrays::default()),
            views: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
        }
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
        let mut tracker = state.batch_tracker.lock().await;
        let mut arrays = state.task_arrays.local.lock().await;
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
        None => state.task_arrays.local.lock().await.clone(),
    }
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
        resources: crate::meat::Resources::new(
            record.template.cpu.map_or(1000, |r| r.request),
            record.template.memory.map_or(64 << 20, |r| r.request),
            0,
        ),
        spec: state.spec.clone(),
        program: record.template.exec.clone().unwrap_or_default(),
        args: record.template.command.clone().unwrap_or_default(),
        env: record
            .template
            .env
            .iter()
            .filter_map(|(key, value)| match value {
                EnvValue::Plain(value) => Some((key.clone(), value.clone())),
                // Refused at submit; never reaches a node.
                EnvValue::Encrypted(_) => None,
            })
            .collect(),
        held,
        stopping: state.stop_reason().is_some(),
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
            .map(|(batch_id, record)| assignment_for(batch_id, record, node))
            .collect(),
    }
}

/// Turn the nodes' answers about one array into its single `Sync` entry
/// for this tick, or `None` if nothing changed. Only results for chunks
/// the node still holds at that attempt are included; grants are planned
/// as if those results had already been applied, so a node that just
/// finished a chunk gets the next one in the same entry.
pub fn plan_sync(
    batch_id: u64,
    record: &TaskArrayRecord,
    answers: &[(NodeId, &ArrayProgress)],
) -> Option<TaskArrayWrite> {
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
    let grants = plan_grants(&preview, &slots);
    if results.is_empty() && grants.is_empty() {
        return None;
    }
    Some(TaskArrayWrite::Sync {
        now_epoch_secs: 0,
        batch_id,
        results,
        grants,
    })
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

async fn leader_tick(state: &ApiState, memory: &mut LeaderMemory) {
    if !is_leading(state).await {
        memory.since = None;
        memory.last_heard.clear();
        memory.last_known.clear();
        memory.failing.clear();
        state.task_arrays.rates.lock().await.clear();
        return;
    }
    let now = Instant::now();
    let since = *memory.since.get_or_insert(now);
    let (arrays, version) = match &state.council {
        Some(council) => {
            let desired = council.desired_state().await;
            let version = super::task_array_node::ControlVersion {
                epoch: desired.recovery_epoch,
                term: council.current_term(),
                index: desired.last_applied_log.map_or(0, |id| id.index),
            };
            (desired.task_arrays, version)
        }
        None => (read_task_arrays(state).await, Default::default()),
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
            eprintln!(
                "bun: task array {batch_id}: {node} can't finish its chunks; requeueing them"
            );
            let write = TaskArrayWrite::Requeue {
                now_epoch_secs: crate::meat::batch_tracker::epoch_now_secs(),
                batch_id,
                node,
            };
            if let Err(error) = write_task_array(state, write).await {
                eprintln!("bun: task array {batch_id}: requeue failed: {error}");
            }
        }
        if let Some(write) = plan_sync(batch_id, record, &for_array)
            && let Err(error) = write_task_array(state, write).await
        {
            eprintln!("bun: task array {batch_id}: sync write failed: {error}");
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
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: JobSpec {
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
            vec![("PLAIN".to_string(), "yes".to_string())]
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
        let write = plan_sync(1, &record, &[(node("a"), &answer)]).unwrap();
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
    fn nothing_to_say_writes_nothing() {
        let mut record = record(20, 10);
        record
            .state
            .grant(&node("a"), &IndexRangeSet::from_range(0..=1))
            .unwrap();
        let answer = progress(4, vec![]);
        assert_eq!(plan_sync(1, &record, &[(node("a"), &answer)]), None);
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
        assert_eq!(plan_sync(1, &record, &[(node("a"), &answer)]), None);
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
}

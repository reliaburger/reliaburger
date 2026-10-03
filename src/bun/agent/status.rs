//! What the agent reports: the status types the API serves, and the loop
//! methods that build them.

use super::*;

/// Result of a deploy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// Number of instances created.
    pub created: usize,
    /// Instance IDs that were created.
    pub instances: Vec<String>,
}

/// One currently deployed resource in the CLI plan's identifier format,
/// served by `GET /v1/apps` for `relish apply --dry-run` diffing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentResourceStatus {
    /// Plan-format identifier: "app.{name}", "job.{name}",
    /// "namespace.{name}" or "permission.{name}".
    pub resource: String,
    /// Image currently deployed, when the resource kind has one.
    pub image: Option<String>,
}

/// Status of a single workload instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// Instance ID.
    pub id: String,
    /// App name.
    pub app_name: String,
    /// Namespace.
    pub namespace: String,
    /// Current lifecycle state.
    pub state: String,
    /// Number of restarts.
    pub restart_count: u32,
    /// Allocated host port, if any.
    pub host_port: Option<u16>,
    /// Exit code of a stopped instance, when the runtime tracks it.
    /// `stopped` alone is ambiguous for jobs — a failing job passes
    /// through `stopped` between retries — so batch watchers (F1) need
    /// this to tell success from failure-in-backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// OS process ID, if available.
    pub pid: Option<u32>,
    /// Some of the runtime's evidence for this instance (its liveness, pid
    /// or exit code) didn't arrive before the status deadline, so a `None`
    /// `pid` or `exit_code` is unknown rather than absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runtime_unknown: bool,
    /// How old the agent loop's published snapshot was when this answer was
    /// read from it, in milliseconds. Never more than two seconds: a node
    /// whose loop hasn't published for longer fails the request instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_ms: Option<u64>,
}

/// A workload status with the node that supplied it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInstanceStatus {
    /// Node name, or `local` for a standalone agent.
    pub node: String,
    /// Node-local workload evidence.
    #[serde(flatten)]
    pub instance: InstanceStatus,
}

/// Status of a run-to-completion job instance, as returned by `/v1/jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,
    pub namespace: String,
    pub instance_id: String,
    pub image: String,
    pub state: String,
    pub restart_count: u32,
    pub age_seconds: u64,
}

/// Status of a single cluster node, as returned by the nodes API.
///
/// Flat, wire-friendly representation of `NodeMembership`. Uses strings
/// instead of newtypes and omits `Instant` fields (not serialisable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Node identifier.
    pub node_id: String,
    /// Node address (gossip endpoint).
    pub address: String,
    /// Agent API endpoint supplied by the cluster's resolved peer directory.
    /// Missing evidence must not be replaced with a guessed port by clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_address: Option<std::net::SocketAddr>,
    /// Current SWIM state: "alive", "suspect", "dead", or "left".
    pub state: String,
    /// SWIM incarnation number.
    pub incarnation: u64,
    /// Whether this node is a council (Raft voter) member.
    pub is_council: bool,
    /// Whether this node is the current Raft leader.
    pub is_leader: bool,
    /// Node labels (zone, region, etc.).
    pub labels: BTreeMap<String, String>,
}

impl NodeStatus {
    /// Whether gossip has given up on this node: declared it dead, or seen it
    /// leave. The listing shows such nodes so people can see them; callers
    /// that want to talk to a node, or run work on it, skip them.
    pub fn is_down(&self) -> bool {
        matches!(self.state.as_str(), "dead" | "left")
    }
}

/// Info about a single council member, as returned by the council API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilMemberInfo {
    /// Raft numeric node ID.
    pub raft_id: u64,
    /// Human-readable node name (maps to `NodeId`).
    pub name: String,
    /// Raft RPC address.
    pub address: String,
    /// Whether the member votes; `false` for a learner still catching up.
    #[serde(default)]
    pub voter: bool,
}

/// The part a node plays in the council, as the node itself sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CouncilRole {
    /// Leads the council.
    Leader,
    /// A voter following the leader.
    Follower,
    /// A voter campaigning for leadership.
    Candidate,
    /// A non-voting member catching up before promotion.
    Learner,
    /// A voter of a council `relish council recover` replaced (#424). It
    /// serves no Raft and refuses writes until it is re-enrolled.
    Fenced,
    /// A restarted voter waiting for its peers' recovery epochs before it
    /// serves Raft.
    Starting,
    /// Not a council member: a worker.
    #[default]
    Worker,
}

impl CouncilRole {
    /// The lowercase name the CLI prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
            Self::Candidate => "candidate",
            Self::Learner => "learner",
            Self::Fenced => "fenced",
            Self::Starting => "starting",
            Self::Worker => "worker",
        }
    }
}

/// Status of the Raft council, as returned by the council API. Every field is
/// the answering node's own view: ask each node to compare them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CouncilStatus {
    /// Council member nodes.
    pub members: Vec<CouncilMemberInfo>,
    /// Current leader node name, if known.
    pub leader: Option<String>,
    /// Current Raft term.
    pub term: u64,
    /// Last applied log index.
    pub last_applied_log: Option<u64>,
    /// Number of registered apps in desired state.
    pub app_count: usize,
    /// The answering node's own role.
    #[serde(default)]
    pub role: CouncilRole,
    /// The recovery epoch the answering node holds, `None` before any
    /// council has admitted it (#424).
    #[serde(default)]
    pub recovery_epoch: Option<u64>,
    /// The newer epoch that fenced the answering node, when it is fenced.
    #[serde(default)]
    pub fenced_by: Option<u64>,
    /// Index of the last entry in the answering node's log.
    #[serde(default)]
    pub last_log_index: Option<u64>,
}

/// Work out a node's council role from its Raft metrics and recovery fence.
pub fn council_role(
    metrics: &openraft::RaftMetrics<u64, crate::council::types::CouncilNodeInfo>,
    fence: Option<crate::council::fence::FenceSnapshot>,
) -> CouncilRole {
    use crate::council::fence::FenceState;
    match fence.map(|fence| fence.state) {
        Some(FenceState::Fenced { .. }) => return CouncilRole::Fenced,
        Some(FenceState::Probing) => return CouncilRole::Starting,
        _ => {}
    }
    let membership = metrics.membership_config.membership();
    if membership.get_node(&metrics.id).is_none() {
        return CouncilRole::Worker;
    }
    if !membership.voter_ids().any(|id| id == metrics.id) {
        return CouncilRole::Learner;
    }
    match metrics.state {
        openraft::ServerState::Leader => CouncilRole::Leader,
        openraft::ServerState::Candidate => CouncilRole::Candidate,
        openraft::ServerState::Follower | openraft::ServerState::Learner => CouncilRole::Follower,
        openraft::ServerState::Shutdown => CouncilRole::Follower,
    }
}

/// This node's own council status, from its council and Raft `metrics`.
/// Reads only local state: no quorum round trip.
pub async fn council_status(
    council: &crate::council::node::CouncilNode,
    metrics: &openraft::RaftMetrics<u64, crate::council::types::CouncilNodeInfo>,
) -> CouncilStatus {
    let desired = council.desired_state().await;
    let membership = metrics.membership_config.membership();
    let leader_name = metrics
        .current_leader
        .and_then(|leader_id| membership.get_node(&leader_id))
        .map(|info| info.name.clone());
    let members = membership
        .nodes()
        .map(|(id, info)| CouncilMemberInfo {
            raft_id: *id,
            name: info.name.clone(),
            address: info.addr.to_string(),
            voter: membership.voter_ids().any(|voter| voter == *id),
        })
        .collect();
    let fence = council.recovery_fence().map(|fence| fence.snapshot());

    CouncilStatus {
        members,
        leader: leader_name,
        term: metrics.current_term,
        last_applied_log: metrics.last_applied.map(|l| l.index),
        app_count: desired.apps.len(),
        role: council_role(metrics, fence),
        recovery_epoch: fence.and_then(|fence| fence.claimed_epoch()),
        fenced_by: fence.and_then(|fence| fence.fenced_by()),
        last_log_index: metrics.last_log_index,
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Handle a snapshot request from the reporting worker.
    pub(super) async fn handle_snapshot_request(&self, req: CollectSnapshotRequest) {
        use crate::reporting::worker::{AgentSnapshot, InstanceSnapshot};

        // The worker gave up on this one; building it would only delay the
        // next live request by another inventory read.
        if req.response.is_closed() {
            return;
        }
        let (capabilities, enforced_instances) = self.live_egress_report_state().await;
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let egress_affected_workloads: Vec<
            crate::reporting::types::EgressAffectedWorkload,
        > = self
            .egress_affected_workloads
            .iter()
            .map(
                |(app_name, namespace)| crate::reporting::types::EgressAffectedWorkload {
                    app_name: app_name.clone(),
                    namespace: namespace.clone(),
                },
            )
            .collect();
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        let egress_affected_workloads = Vec::new();

        // The report deadline is two seconds. Bound the evidence read without
        // hiding capacity when inventory is unavailable or internally ambiguous.
        let launches = match self
            .runtime_inventory(LOOP_RUNTIME_INVENTORY_TIMEOUT, BunError::AdoptionState)
            .await
        {
            Ok(Some(launches)) => {
                let count = launches.len();
                let by_instance: std::collections::HashMap<_, _> = launches
                    .into_iter()
                    .map(|launch| (launch.instance_id.clone(), launch))
                    .collect();
                (by_instance.len() == count).then_some(by_instance)
            }
            _ => None,
        };
        let instances = self.supervisor.list_instances();
        // LOOP-INLINE: in-memory lock, no I/O
        let snapshot = AgentSnapshot {
            instances: instances
                .iter()
                .map(|inst| {
                    // The report carries the replica ordinal, recovered from
                    // the canonical id (e.g. "default__web-0" → 0).
                    let instance_id = crate::grill::InstanceIdentity::parse(&inst.id.0)
                        .map(|ident| ident.ordinal)
                        .unwrap_or(0);

                    // Requested resources from the deployed spec: these
                    // are the commitments the scheduler must respect.
                    let spec = self
                        .deployed_specs
                        .get(&(inst.app_name.clone(), inst.namespace.clone()));
                    let cpu_request_millicores = spec
                        .and_then(|s| s.cpu.as_ref())
                        .map(|r| r.request as u32)
                        .unwrap_or(0);
                    let memory_request_mb = spec
                        .and_then(|s| s.memory.as_ref())
                        .map(|r| (r.request / (1024 * 1024)) as u32)
                        .unwrap_or(0);
                    let has_egress = spec
                        .and_then(|s| s.egress.as_ref())
                        .is_some_and(|e| !e.allow.is_empty() || !e.allow_franchise.is_empty());
                    let egress_enforcement = if !has_egress {
                        crate::reporting::types::EgressEnforcementStatus::NotRequested
                    } else if capabilities.egress.can_enforce_allowlist()
                        && enforced_instances.contains(&inst.id)
                    {
                        crate::reporting::types::EgressEnforcementStatus::Enforced
                    } else {
                        crate::reporting::types::EgressEnforcementStatus::Unenforced
                    };

                    InstanceSnapshot {
                        execution: launches
                            .as_ref()
                            .and_then(|known| known.get(&inst.id))
                            .filter(|launch| {
                                inst.oci_spec
                                    .as_ref()
                                    .is_some_and(|spec| launch.launched(spec))
                            })
                            .map(|launch| crate::grill::RuntimeExecution {
                                instance_id: launch.instance_id.clone(),
                                generation: launch.generation.clone(),
                            }),
                        app_name: inst.app_name.clone(),
                        namespace: inst.namespace.clone(),
                        instance_id,
                        image: inst.image.clone(),
                        port: inst.host_port,
                        container_state: inst.state,
                        consecutive_unhealthy: inst.health_counters.consecutive_unhealthy,
                        uptime: inst.created_at.elapsed(),
                        cpu_request_millicores,
                        memory_request_mb,
                        egress_enforcement,
                    }
                })
                .collect(),
            // Terminal instances no longer hold their ports (CP6) — the
            // worker also filters them from running/capacity.
            allocated_ports: instances
                .iter()
                .filter(|i| {
                    !matches!(
                        i.state,
                        crate::grill::state::ContainerState::Stopped
                            | crate::grill::state::ContainerState::Failed
                    )
                })
                .filter_map(|i| i.host_port)
                .collect(),
            capacity_cpu_millicores: self.capacity_cpu_millicores,
            capacity_memory_mb: self.capacity_memory_mb,
            capabilities,
            readiness: match &self.readiness {
                Some(readiness) => Some(readiness.snapshot().await),
                None => None,
            },
            egress_degraded: !egress_affected_workloads.is_empty(),
            egress_affected_workloads,
        };
        let _ = req.response.send(snapshot);
    }

    /// Get cluster node membership from gossip, or empty if single-node.
    ///
    /// Lists gossip's live members (alive and suspect), then each of `down`
    /// that gossip doesn't list any more, so a dead node shows as dead
    /// instead of disappearing.
    pub(super) fn get_cluster_nodes(&self, down: Vec<NodeStatus>) -> Vec<NodeStatus> {
        let Some(handle) = &self.cluster else {
            return Vec::new();
        };

        // Cross-reference the Raft council so the COUNCIL / LEADER columns
        // reflect actual consensus state. The gossip-level `is_council` /
        // `is_leader` flags on the membership snapshot are never set by this
        // runtime — council membership and leadership live in the Raft metrics.
        // A node is a council member if it's a current voter, and the leader if
        // it's the current Raft leader.
        let mut council_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut leader_name: Option<String> = None;
        if let Some(metrics_rx) = &handle.raft_metrics_rx {
            let metrics = metrics_rx.borrow();
            let membership = metrics.membership_config.membership();
            council_names = membership
                .voter_ids()
                .filter_map(|id| membership.get_node(&id).map(|n| n.name.clone()))
                .collect();
            leader_name = metrics
                .current_leader
                .and_then(|id| membership.get_node(&id).map(|n| n.name.clone()));
        }

        let have_metrics = handle.raft_metrics_rx.is_some();
        // Raft metrics are authoritative when the council is wired;
        // otherwise fall back to whatever the gossip snapshot reports. A
        // dead voter is still a voter, so down members get the same check.
        let roles = |name: &str, gossip_council: bool, gossip_leader: bool| {
            if have_metrics {
                (
                    council_names.contains(name),
                    leader_name.as_deref() == Some(name),
                )
            } else {
                (gossip_council, gossip_leader)
            }
        };
        let membership = handle.membership_rx.borrow();
        let mut nodes: Vec<NodeStatus> = membership
            .iter()
            .map(|m| {
                let name = m.node_id.to_string();
                let (is_council, is_leader) = roles(&name, m.is_council, m.is_leader);
                NodeStatus {
                    node_id: name,
                    address: m.address.to_string(),
                    api_address: None,
                    state: m.state.to_string(),
                    incarnation: m.incarnation,
                    is_council,
                    is_leader,
                    labels: m.labels.clone(),
                }
            })
            .collect();
        for mut node in down {
            if nodes.iter().any(|live| live.node_id == node.node_id) {
                continue;
            }
            (node.is_council, node.is_leader) = roles(&node.node_id, false, false);
            nodes.push(node);
        }
        nodes
    }

    /// Get Raft council status, or default if single-node/non-council.
    pub(super) async fn get_council_status(&self) -> CouncilStatus {
        let Some(handle) = &self.cluster else {
            return CouncilStatus::default();
        };
        let Some(council) = &handle.council else {
            return CouncilStatus::default();
        };
        let Some(metrics_rx) = &handle.raft_metrics_rx else {
            return CouncilStatus::default();
        };

        let metrics = metrics_rx.borrow().clone();
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
        council_status(council, &metrics).await
    }

    /// Collect cluster node IPs from the gossip membership table.
    pub(super) fn collect_cluster_node_ips(&self) -> crate::firewall::rules::ClusterNodes {
        let mut nodes = crate::firewall::rules::ClusterNodes::new();

        if let Some(ref cluster) = self.cluster {
            let membership = cluster.membership_rx.borrow();
            for snapshot in membership.iter() {
                nodes.insert(snapshot.address.ip());
            }
        }

        nodes
    }

    /// What the loop knows about every instance, for a status snapshot.
    pub(super) fn status_entries(&self) -> Vec<status_snapshot::StatusEntry> {
        use status_snapshot::{EvidenceSource, StatusEntry};
        self.supervisor
            .list_instances()
            .into_iter()
            .map(|instance| {
                let recorded_exit =
                    match self.recorded_jobs.get(&instance.id.0).map(|job| &job.phase) {
                        Some(crate::bun::jobs::JobPhase::Exited { code }) => Some(Some(*code)),
                        Some(crate::bun::jobs::JobPhase::Unknown) => Some(None),
                        _ => None,
                    };
                let evidence = if instance.is_being_created() {
                    EvidenceSource::Creating {
                        exit_code: recorded_exit.flatten(),
                    }
                } else {
                    EvidenceSource::Runtime {
                        recorded_exit,
                        alive: matches!(
                            instance.state,
                            ContainerState::Running
                                | ContainerState::HealthWait
                                | ContainerState::Unhealthy
                        ),
                    }
                };
                StatusEntry {
                    status: InstanceStatus {
                        id: instance.id.0.clone(),
                        app_name: instance.app_name.clone(),
                        namespace: instance.namespace.clone(),
                        state: self.job_state_label(instance),
                        restart_count: instance.restart_count,
                        host_port: instance.host_port,
                        exit_code: None,
                        pid: None,
                        runtime_unknown: false,
                        status_age_ms: None,
                    },
                    evidence,
                }
            })
            .collect()
    }

    /// Publish what the loop knows now, for status readers, and return it.
    pub(super) fn publish_status(&self) -> Arc<status_snapshot::StatusSnapshot> {
        let snapshot = Arc::new(status_snapshot::StatusSnapshot::new(self.status_entries()));
        self.status_tx.send_replace(Arc::clone(&snapshot));
        snapshot
    }

    /// A reader that answers status requests from the snapshot this agent's
    /// loop publishes, without queueing anything for the loop.
    pub fn status_reader(&self) -> status_snapshot::StatusReader {
        status_snapshot::StatusReader::new(
            self.status_tx.subscribe(),
            self.supervisor.grill().clone(),
        )
    }

    /// Every instance's status, read the way a status request reads it:
    /// published now, then completed with the runtime's evidence under the
    /// shared `STATUS_RUNTIME_READ_TIMEOUT`.
    #[cfg(test)]
    pub(super) async fn get_status(&self) -> Vec<InstanceStatus> {
        let snapshot = self.publish_status();
        status_snapshot::read_status(self.supervisor.grill(), &snapshot).await
    }

    pub(super) fn get_job_status(&self) -> Vec<JobStatus> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.is_job)
            .map(|instance| JobStatus {
                name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                instance_id: instance.id.0.clone(),
                image: instance.image.clone(),
                state: self.job_state_label(instance),
                restart_count: instance.restart_count,
                age_seconds: instance.created_at.elapsed().as_secs(),
            })
            .collect()
    }
}

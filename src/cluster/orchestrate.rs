//! Cluster orchestration: the leader scheduling loop and the per-node
//! placement reconciler (Stage 4 W6, L1).
//!
//! The design is desired-state-driven, not push-RPC: `relish apply`
//! commits `AppSpec`s to Raft; the leader schedules them into
//! `SchedulingDecision`s (also in Raft); every node polls the leader
//! for "what is assigned to me" and reconciles its local instances to
//! match. Reconciliation is idempotent, so crashes, retries, and
//! leadership changes self-heal — there is no per-instance RPC whose
//! failure needs bespoke bookkeeping.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bun::agent::{AgentCommand, ApplyEvent};
use crate::config::app::AppSpec;
use crate::config::{Config, Replicas};
use crate::council::node::CouncilNode;
use crate::council::types::{CouncilNodeInfo, RaftRequest};
use crate::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};
use crate::meat::scheduler::Scheduler;
use crate::meat::types::{NodeId, Resources};
use crate::mustard::membership::MembershipSnapshot;
use crate::mustard::state::NodeState;
use crate::reporting::aggregator::AggregatedState;

/// How often the leader re-evaluates scheduling and nodes poll their
/// assignments.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// One app assigned to a node, as served by `/v1/placements/{node}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAssignment {
    pub name: String,
    pub namespace: String,
    /// Number of replicas of this app assigned to the node.
    pub replicas: u32,
    pub spec: AppSpec,
}

/// The full assignment list for a node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeAssignments {
    pub apps: Vec<NodeAssignment>,
}

/// Spawn the leader's scheduling loop.
///
/// Leader-only (checked every tick, so leadership changes need no
/// start/stop dance). For every desired app whose placements are
/// missing, sized wrongly, or reference dead nodes, runs the Phase-2
/// scheduler over live capacity data and proposes the new
/// `SchedulingDecision` to Raft.
pub fn spawn_leader_scheduler(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    aggregated_rx: watch::Receiver<AggregatedState>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            if !council.is_leader().await {
                continue;
            }

            let desired = council.desired_state().await;
            let members = membership_rx.borrow().clone();
            let reports = aggregated_rx.borrow().clone();

            let alive: HashSet<NodeId> = members
                .iter()
                .filter(|m| m.state == NodeState::Alive)
                .map(|m| m.node_id.clone())
                .collect();
            if alive.is_empty() {
                continue;
            }

            for (app_id, spec) in &desired.apps {
                let want = match spec.replicas {
                    Replicas::Fixed(n) => n as usize,
                    Replicas::DaemonSet => alive.len(),
                };
                let placements_ok = desired
                    .scheduling
                    .get(app_id)
                    .map(|placements| {
                        placements.len() == want
                            && placements.iter().all(|p| alive.contains(&p.node_id))
                    })
                    .unwrap_or(false);
                if placements_ok {
                    continue;
                }

                let cache = build_cluster_cache(&members, &reports);
                if cache.node_count() == 0 {
                    // No node has reported capacity yet; scheduling
                    // against unknown capacity would place blindly.
                    continue;
                }
                let mut scheduler = Scheduler::new(cache);
                match scheduler.schedule_app(app_id, spec) {
                    Ok(decision) => {
                        if let Err(e) = council
                            .write(RaftRequest::SchedulingDecision(decision))
                            .await
                        {
                            eprintln!("scheduler: failed to commit placement for {app_id}: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("scheduler: cannot place {app_id}: {e}");
                    }
                }
            }
        }
    });
}

/// Build the scheduler's view of the cluster from gossip membership
/// (who is alive, labels, age) and aggregated reports (capacity and
/// current commitments — real numbers since the reporting wiring).
///
/// Nodes without a report yet are omitted: scheduling against unknown
/// capacity is guessing.
fn build_cluster_cache(
    members: &[MembershipSnapshot],
    reports: &AggregatedState,
) -> ClusterStateCache {
    let mut cache = ClusterStateCache::new();
    for member in members {
        if member.state != NodeState::Alive {
            continue;
        }
        let Some(report) = reports.reports.get(&member.node_id) else {
            continue;
        };
        let usage = &report.resource_usage;
        if usage.cpu_total_millicores == 0 {
            continue; // pre-capacity node (or capacity unset)
        }

        let running_apps = report
            .running_apps
            .iter()
            .map(|a| crate::meat::types::AppId::new(&a.app_name, &a.namespace))
            .collect();

        cache.set_node(SchedulerNodeState {
            node_id: member.node_id.clone(),
            allocatable: Resources {
                cpu_millicores: u64::from(usage.cpu_total_millicores),
                memory_bytes: u64::from(usage.memory_total_mb) * 1024 * 1024,
                gpus: 0,
            },
            allocated: Resources {
                cpu_millicores: u64::from(usage.cpu_used_millicores),
                memory_bytes: u64::from(usage.memory_used_mb) * 1024 * 1024,
                gpus: 0,
            },
            labels: member.labels.clone(),
            ready: true,
            running_apps,
            uptime_secs: member.first_seen.elapsed().as_secs(),
            // Nothing reports cached images yet; locality scoring is
            // inert rather than fed guesses.
            cached_images: HashSet::new(),
        });
    }
    cache
}

/// Spawn the per-node placement reconciler.
///
/// Polls the leader's `/v1/placements/{node}` endpoint and converges
/// local instances: deploys apps whose assignment appeared or changed,
/// stops apps whose assignment disappeared. Skips no-op cycles by
/// remembering the last applied assignment per app.
pub fn spawn_placement_reconciler(
    node_name: String,
    metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    // API port relative to the raft port (ports are uniform ACROSS
    // nodes only as offsets — single-host clusters, like the tests,
    // give every node its own port block). Same derivation idiom as
    // the runtime's raft/reporting offsets.
    raft_to_api_offset: i32,
    service_token: Option<String>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        // (name, namespace) → serialized assignment we last applied.
        let mut applied: BTreeMap<(String, String), String> = BTreeMap::new();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }

            // Leader's API address, derived from its raft address by
            // the fixed offset. Nodes outside the council don't learn
            // a leader from Raft metrics and skip — same standing
            // limitation as the reporting tree (flat-star,
            // ≤ council-size clusters fully covered).
            let leader_url = {
                let m = metrics_rx.borrow();
                m.current_leader.and_then(|leader_id| {
                    m.membership_config
                        .membership()
                        .get_node(&leader_id)
                        .map(|info| {
                            let api_port = (info.addr.port() as i32 + raft_to_api_offset) as u16;
                            format!("http://{}:{api_port}", info.addr.ip())
                        })
                })
            };
            let Some(leader_url) = leader_url else {
                continue;
            };

            let url = format!("{leader_url}/v1/placements/{node_name}");
            let mut request = client.get(&url);
            if let Some(token) = &service_token {
                request = request.bearer_auth(token);
            }
            let assignments: NodeAssignments = match request.send().await {
                Ok(response) if response.status().is_success() => match response.json().await {
                    Ok(a) => a,
                    Err(_) => continue,
                },
                _ => continue, // leader unreachable; retry next tick
            };

            let mut seen: HashSet<(String, String)> = HashSet::new();
            for assignment in &assignments.apps {
                let key = (assignment.name.clone(), assignment.namespace.clone());
                seen.insert(key.clone());

                let mut spec = assignment.spec.clone();
                // The local agent runs exactly this node's share.
                spec.replicas = Replicas::Fixed(assignment.replicas);
                let fingerprint = serde_json::to_string(&spec).unwrap_or_default();
                if applied.get(&key) == Some(&fingerprint) {
                    continue; // already converged; don't redeploy
                }

                let mut config = Config::default();
                config.app.insert(assignment.name.clone(), spec);

                // Drain the deploy events; the reconciler's outcome is
                // visible through /v1/status, not a console.
                let (event_tx, mut event_rx) = mpsc::channel::<ApplyEvent>(32);
                tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
                if cmd_tx
                    .send(AgentCommand::Deploy {
                        config,
                        events: event_tx,
                    })
                    .await
                    .is_ok()
                {
                    applied.insert(key, fingerprint);
                }
            }

            // Anything we applied before that is no longer assigned to
            // this node gets stopped.
            let removed: Vec<(String, String)> = applied
                .keys()
                .filter(|key| !seen.contains(*key))
                .cloned()
                .collect();
            for (name, namespace) in removed {
                let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx
                    .send(AgentCommand::Stop {
                        app_name: name.clone(),
                        namespace: namespace.clone(),
                        response: response_tx,
                    })
                    .await;
                applied.remove(&(name, namespace));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::types::{ResourceUsage, StateReport};
    use std::collections::HashMap;
    use std::time::{Instant, SystemTime};

    fn member(name: &str, port: u16) -> MembershipSnapshot {
        MembershipSnapshot {
            node_id: NodeId::new(name),
            address: format!("127.0.0.1:{port}").parse().unwrap(),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
            first_seen: Instant::now(),
            resources: None,
        }
    }

    fn report(cpu_total: u32, cpu_used: u32) -> StateReport {
        StateReport {
            node_id: NodeId::new("x"),
            timestamp: SystemTime::now(),
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage {
                cpu_used_millicores: cpu_used,
                memory_used_mb: 256,
                disk_used_mb: 0,
                gpu_used: 0,
                allocated_ports: vec![],
                cpu_total_millicores: cpu_total,
                memory_total_mb: 8192,
            },
            event_log: vec![],
        }
    }

    #[test]
    fn cache_includes_only_alive_nodes_with_reported_capacity() {
        let mut dead = member("dead", 2);
        dead.state = NodeState::Dead;
        let members = vec![member("a", 1), dead, member("unreported", 3)];

        let mut reports = AggregatedState {
            reports: HashMap::new(),
            stale_nodes: vec![],
        };
        reports.reports.insert(NodeId::new("a"), report(4000, 250));
        // "dead" has a report but isn't alive; "unreported" is alive
        // but has no report — both are excluded.
        reports.reports.insert(NodeId::new("dead"), report(4000, 0));

        let cache = build_cluster_cache(&members, &reports);
        assert_eq!(cache.node_count(), 1);
        let node = cache.get_node(&NodeId::new("a")).unwrap();
        assert_eq!(node.allocatable.cpu_millicores, 4000);
        assert_eq!(node.allocated.cpu_millicores, 250);
    }

    #[test]
    fn cache_skips_zero_capacity_reports() {
        let members = vec![member("zero", 1)];
        let mut reports = AggregatedState {
            reports: HashMap::new(),
            stale_nodes: vec![],
        };
        reports.reports.insert(NodeId::new("zero"), report(0, 0));

        let cache = build_cluster_cache(&members, &reports);
        assert_eq!(cache.node_count(), 0);
    }
}

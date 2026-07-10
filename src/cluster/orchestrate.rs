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

/// Spawn the leader's scheduling loop, with state reconstruction (L4).
///
/// Leader-only (checked every tick, so leadership changes need no
/// start/stop dance). A freshly-elected leader first runs a *learning
/// period*: it waits for enough workers to report their actual running
/// apps before it schedules anything. Without this, a new leader would
/// re-place apps that are already running but haven't reported yet,
/// duplicating workloads. Once the learning period completes (coverage
/// threshold met, or timeout), the loop schedules as normal: for every
/// desired app whose placements are missing, sized wrongly, or on dead
/// nodes, it runs the scheduler and commits a `SchedulingDecision`.
/// Nodes that never reported (`UnknownNode` corrections) are excluded
/// from placement until they do.
pub fn spawn_leader_scheduler(
    council: Arc<CouncilNode>,
    membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    aggregated_rx: watch::Receiver<AggregatedState>,
    reconstruction_config: crate::config::node::ReconstructionSection,
    shutdown: CancellationToken,
) {
    use crate::reconstruction::controller::ReconstructionController;
    use crate::reconstruction::types::ReconstructionPhase;

    tokio::spawn(async move {
        let mut reconstruction = ReconstructionController::new(reconstruction_config);
        let mut was_leader = false;
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }

            let is_leader = council.is_leader().await;
            // Leadership edges drive the reconstruction state machine.
            if is_leader && !was_leader {
                let alive_count = membership_rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == NodeState::Alive)
                    .count();
                reconstruction.on_leader_elected(alive_count);
            } else if !is_leader && was_leader {
                reconstruction.on_leader_lost();
            }
            was_leader = is_leader;
            if !is_leader {
                continue;
            }

            let desired = council.desired_state().await;
            let members = membership_rx.borrow().clone();
            let reports = aggregated_rx.borrow().clone();

            let alive: Vec<NodeId> = members
                .iter()
                .filter(|m| m.state == NodeState::Alive)
                .map(|m| m.node_id.clone())
                .collect();
            if alive.is_empty() {
                continue;
            }

            // Learning period: feed reports in, check the timeout, and
            // do NOT schedule until reconstruction reaches Active. This
            // is the L4 gate — a fresh leader waits for workers to report
            // what they're actually running before it schedules, so it
            // never re-places an app that's already running but hasn't
            // reported yet. Once Active, scheduling proceeds over all
            // alive nodes; a node that reports late is simply picked up
            // by the ordinary scheduler on a later tick.
            if reconstruction.phase() != ReconstructionPhase::Active {
                reconstruction.on_report_received(&reports, &desired, &alive);
                reconstruction.check_timeout(&desired, &alive, &reports);
                if reconstruction.phase() != ReconstructionPhase::Active {
                    continue; // still learning — hold off on scheduling
                }
            }

            let alive: HashSet<NodeId> = alive.into_iter().collect();

            for (app_id, spec) in &desired.apps {
                // Effective replicas = autoscale override (L3) if the
                // autoscaler set one, else the spec/config baseline.
                let override_replicas = desired
                    .autoscale_overrides
                    .iter()
                    .find(|(k, _)| k == &app_id.to_string())
                    .map(|(_, n)| *n);
                let want = effective_replicas(spec, override_replicas, alive.len());
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
                // Feed the scheduler the effective replica count.
                let mut effective_spec = spec.clone();
                if let Some(n) = override_replicas {
                    effective_spec.replicas = Replicas::Fixed(n);
                }
                let mut scheduler = Scheduler::new(cache);
                match scheduler.schedule_app(app_id, &effective_spec) {
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

/// The replica count the scheduler should target for an app: the
/// autoscale override if the autoscaler set one, else the spec's own
/// count (daemon sets fan out to every alive node).
fn effective_replicas(spec: &AppSpec, override_replicas: Option<u32>, alive_count: usize) -> usize {
    match (override_replicas, spec.replicas) {
        (Some(n), _) => n as usize,
        (None, Replicas::Fixed(n)) => n as usize,
        (None, Replicas::DaemonSet) => alive_count,
    }
}

/// Spawn the leader's autoscale loop (L3).
///
/// Every `interval`, on the leader, for each app with an
/// `[autoscale]` section: query the app's metric from the rollup store,
/// run the (tested) `evaluate` decision function, and commit an
/// `AutoscaleOverride` to Raft when a scale is warranted. The scheduler
/// then re-places at the new replica count.
///
/// The library's `run_autoscale_loop` takes a *synchronous*
/// `app_provider` closure, which can't read async Raft desired state or
/// the rollup store; this task drives the same pure functions
/// (`AutoscaleConfig::from_spec`, `evaluate`, `AutoscaleTracker`)
/// directly instead.
pub fn spawn_autoscaler(
    council: Arc<CouncilNode>,
    rollup_store: Arc<tokio::sync::RwLock<crate::mayo::rollup_store::RollupStore>>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    use crate::meat::autoscaler::{AutoscaleConfig, AutoscaleTracker, evaluate};

    tokio::spawn(async move {
        let mut tracker = AutoscaleTracker::default();
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            if !council.is_leader().await {
                continue;
            }

            let desired = council.desired_state().await;
            for (app_id, spec) in &desired.apps {
                let Some(autoscale) = &spec.autoscale else {
                    continue;
                };
                let Some(config) = AutoscaleConfig::from_spec(autoscale) else {
                    continue;
                };

                // Baseline the tracker at the current effective replica
                // count (override if one exists, else the spec's).
                let baseline = desired
                    .autoscale_overrides
                    .iter()
                    .find(|(k, _)| k == &app_id.to_string())
                    .map(|(_, n)| *n)
                    .unwrap_or(match spec.replicas {
                        Replicas::Fixed(n) => n,
                        Replicas::DaemonSet => continue, // daemon sets don't autoscale
                    });

                // Metric utilisation for this app over the last window.
                let Some(metric) =
                    app_metric_utilisation(&rollup_store, &config.metric, &app_id.name).await
                else {
                    continue; // no data yet
                };

                let now = std::time::Instant::now();
                let state = tracker.get_or_insert(app_id, baseline);
                // Keep the tracker's current in sync with cluster truth
                // (another leader may have scaled while we were a follower).
                state.current_replicas = baseline;
                if let Some(decision) = evaluate(app_id, &config, state, metric, now) {
                    tracker.apply_decision(&decision, now);
                    if let Err(e) = council
                        .write(RaftRequest::AutoscaleOverride {
                            app_id: app_id.clone(),
                            replicas: decision.to,
                            reason: decision.reason.clone(),
                        })
                        .await
                    {
                        eprintln!("autoscaler: failed to commit override for {app_id}: {e}");
                    }
                }
            }
        }
    });
}

/// Average utilisation of `metric` for `app` over the recent window,
/// as a fraction, from the leader's rollup store.
///
/// Returns `None` when there's no data. The value is interpreted as a
/// utilisation fraction (0.0–1.0) to compare against the autoscale
/// target; the metric Mayo records must be scaled accordingly.
async fn app_metric_utilisation(
    rollup_store: &tokio::sync::RwLock<crate::mayo::rollup_store::RollupStore>,
    metric: &str,
    app: &str,
) -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let window_start = now.saturating_sub(300); // last 5 minutes

    let store = rollup_store.read().await;
    let aggregates = store
        .query_cluster_aggregates(metric, window_start, now)
        .await
        .ok()?;
    // Filter to rows whose labels mention this app, and average the
    // per-timestamp means (sum/count).
    let mut total = 0.0;
    let mut n = 0u64;
    for agg in &aggregates {
        if agg.labels.contains(app) && agg.count > 0 {
            total += agg.sum / f64::from(agg.count);
            n += 1;
        }
    }
    if n == 0 { None } else { Some(total / n as f64) }
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
            has_buildah: false,
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

    fn spec_from_toml(body: &str) -> AppSpec {
        let config = crate::config::Config::parse(body).unwrap();
        config.app.into_values().next().unwrap()
    }

    #[test]
    fn effective_replicas_prefers_the_autoscale_override() {
        let spec = spec_from_toml("[app.web]\nimage = \"x:1\"\nreplicas = 2\n");
        // No override: the spec's own count.
        assert_eq!(effective_replicas(&spec, None, 5), 2);
        // Override: wins over the spec (L3 — this is how a scale takes
        // effect; the scheduler re-places at the override count).
        assert_eq!(effective_replicas(&spec, Some(4), 5), 4);
    }

    #[test]
    fn effective_replicas_daemonset_fans_out() {
        let spec = spec_from_toml("[app.web]\nimage = \"x:1\"\nreplicas = \"*\"\n");
        assert_eq!(effective_replicas(&spec, None, 3), 3);
    }

    #[tokio::test]
    async fn app_metric_utilisation_averages_the_app_rows() {
        use crate::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};
        use crate::mayo::rollup_store::RollupStore;
        use std::collections::BTreeMap as Map;

        let dir = tempfile::tempdir().unwrap();
        let store = tokio::sync::RwLock::new(RollupStore::new(dir.path().to_path_buf()));
        {
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let mut labels = Map::new();
            labels.insert("app".to_string(), "web".to_string());
            let rollup = NodeRollup {
                node_id: NodeId::new("n1"),
                timestamp: now.saturating_sub(60),
                entries: vec![RollupEntry {
                    metric_name: "cpu".to_string(),
                    labels,
                    // sum 1.6 over 2 samples → mean 0.8 utilisation.
                    aggregate: RollupAggregate {
                        min: 0.7,
                        max: 0.9,
                        sum: 1.6,
                        count: 2,
                    },
                }],
            };
            let mut w = store.write().await;
            w.ingest(&rollup);
            w.flush().await.unwrap();
        }

        let value = app_metric_utilisation(&store, "cpu", "web").await;
        assert!(
            value.is_some_and(|v| (v - 0.8).abs() < 1e-9),
            "expected mean utilisation 0.8, got {value:?}"
        );

        // Unknown app → no data.
        assert!(
            app_metric_utilisation(&store, "cpu", "other")
                .await
                .is_none()
        );
    }
}

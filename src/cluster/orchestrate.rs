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

            // Build the cache ONCE per tick, cordon mid-upgrade nodes, and
            // plan every app against a single mutable reservation view so
            // apps planned in the same pass reserve against each other. The
            // old code rebuilt a fresh cache per app, so two apps that
            // together exceeded one node's headroom both landed on it — a
            // cache you rebuild per decision is a cache that lies between
            // decisions (CP8).
            let mut cache = build_cluster_cache(&members, &reports);
            if cache.node_count() == 0 {
                // No node has reported capacity yet; scheduling against
                // unknown capacity would place blindly.
                continue;
            }
            crate::meat::filter::apply_upgrade_cordon(&mut cache, desired.active_upgrade.as_ref());

            // Quotas come from desired-state namespaces (12b.2 T6). A
            // cluster with no declared namespace budgets gets an empty
            // ledger that admits everything; declaring a namespace with a
            // CPU/memory/replica cap now rejects over-budget placements at
            // deploy time, with the reason surfaced through the log.
            let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);

            let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);

            for decision in decisions {
                // Revalidate against the LATEST membership before the async
                // Raft write: a node that died between planning and commit
                // (or between two commits in this pass) must not receive the
                // placement. `members`/`reports` were snapshotted at the top
                // of the tick; membership can move under a slow write.
                let live: HashSet<NodeId> = membership_rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == NodeState::Alive)
                    .map(|m| m.node_id.clone())
                    .collect();
                if !decision
                    .placements
                    .iter()
                    .all(|p| live.contains(&p.node_id))
                {
                    eprintln!(
                        "scheduler: dropping stale placement for {} (a target node left mid-pass)",
                        decision.app_id
                    );
                    continue;
                }
                if let Err(e) = council
                    .write(RaftRequest::SchedulingDecision(decision.clone()))
                    .await
                {
                    eprintln!(
                        "scheduler: failed to commit placement for {}: {e}",
                        decision.app_id
                    );
                }
            }
        }
    });
}

/// Plan placements for one scheduling tick against a single mutable
/// reservation cache.
///
/// For every desired app whose current placements are missing, wrongly
/// sized, or on a now-ineligible node, this runs the scheduler and
/// records the decision. Each committed decision reserves its resources
/// in `cache` before the next app plans, so apps in the same pass reserve
/// against each other (the CP8 double-booking fix). Daemon apps converge
/// against the currently eligible nodes: they gain an instance as a node
/// becomes eligible and lose one as a node leaves or is cordoned, because
/// the scheduler re-plans a daemon set over the live filtered node list
/// each tick.
///
/// `quotas` gates admission cumulatively per namespace (empty in
/// production until T6 feeds it).
fn plan_scheduling_pass(
    cache: &mut ClusterStateCache,
    desired: &crate::council::types::DesiredState,
    alive: &HashSet<NodeId>,
    quotas: &mut crate::meat::quota::QuotaLedger,
) -> Vec<crate::meat::types::SchedulingDecision> {
    use crate::meat::scheduler::Scheduler;

    let mut decisions = Vec::new();
    // A stable order so a pass is deterministic (HashMap iteration isn't).
    let mut app_ids: Vec<_> = desired.apps.keys().cloned().collect();
    app_ids.sort_by_key(|a| a.to_string());

    for app_id in &app_ids {
        let Some(spec) = desired.apps.get(app_id) else {
            continue;
        };
        let override_replicas = desired
            .autoscale_overrides
            .iter()
            .find(|(k, _)| k == &app_id.to_string())
            .map(|(_, n)| *n);
        let want = effective_replicas(spec, override_replicas, alive.len());

        // Already converged? A placement is only OK if it targets a node
        // that is still alive (a departed/cordoned node's placement is
        // stale and forces a re-plan — daemon convergence and node loss).
        let placements_ok = desired
            .scheduling
            .get(app_id)
            .map(|placements| {
                placements.len() == want && placements.iter().all(|p| alive.contains(&p.node_id))
            })
            .unwrap_or(false);
        if placements_ok {
            continue;
        }

        // Quota admission (cumulative within the pass). A rejection is a
        // deploy-time error surfaced through the log, not a silent skip
        // that leaves the app forever pending without explanation.
        let per_replica = scheduler_resources(spec);
        let is_new_app = !desired.scheduling.contains_key(app_id);
        if !quotas.is_empty()
            && let Err(e) =
                quotas.try_admit(&app_id.namespace, &per_replica, want as u32, is_new_app)
        {
            eprintln!("scheduler: quota rejects {app_id}: {e}");
            continue;
        }

        // Feed the scheduler the effective replica count. The scheduler
        // reserves into the SHARED cache, so the next app sees this app's
        // footprint.
        let mut effective_spec = spec.clone();
        if let Some(n) = override_replicas {
            effective_spec.replicas = Replicas::Fixed(n);
        }
        // The scheduler owns its cache, so hand it the shared one and take
        // it back afterwards (Rust move semantics — no shared &mut alias).
        let mut scheduler = Scheduler::new(std::mem::take(cache));
        let result = scheduler.schedule_app(app_id, &effective_spec);
        *cache = scheduler.cluster;
        match result {
            Ok(decision) => decisions.push(decision),
            Err(e) => eprintln!("scheduler: cannot place {app_id}: {e}"),
        }
    }
    decisions
}

/// The per-replica resources an app requests, for quota accounting. Mirrors
/// the scheduler's `extract_resources` (request values, zero when unset).
fn scheduler_resources(spec: &AppSpec) -> Resources {
    Resources::new(
        spec.cpu.as_ref().map(|r| r.request).unwrap_or(0),
        spec.memory.as_ref().map(|r| r.request).unwrap_or(0),
        spec.gpu.unwrap_or(0),
    )
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
                let config = match AutoscaleConfig::from_spec(autoscale) {
                    Ok(config) => config,
                    Err(e) => {
                        // Config validation catches this on apply, so a bad
                        // block shouldn't reach here — log and skip if it does.
                        eprintln!("autoscaler: invalid [autoscale] for {app_id}: {e}");
                        continue;
                    }
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

                // Metric utilisation for this app over the CONFIGURED window
                // (was hardcoded to five minutes regardless of the spec).
                let Some(metric) = app_metric_utilisation(
                    &rollup_store,
                    &config.metric,
                    &app_id.name,
                    config.evaluation_window,
                )
                .await
                else {
                    continue; // no data yet
                };

                let now = std::time::Instant::now();
                let state = tracker.get_or_insert(app_id, baseline);
                // Keep the tracker's current in sync with cluster truth
                // (another leader may have scaled while we were a follower).
                state.current_replicas = baseline;
                if let Some(decision) = evaluate(app_id, &config, state, metric, now) {
                    // Commit FIRST, start the cooldown only after the write
                    // succeeds (DEP8). The old code applied the decision —
                    // and thus started the cooldown — before the Raft write,
                    // so a failed write would suppress the next real attempt
                    // for a whole cooldown while nothing actually scaled.
                    match council
                        .write(RaftRequest::AutoscaleOverride {
                            app_id: app_id.clone(),
                            replicas: decision.to,
                            reason: decision.reason.clone(),
                        })
                        .await
                    {
                        Ok(_) => tracker.apply_decision(&decision, now),
                        Err(e) => {
                            eprintln!("autoscaler: failed to commit override for {app_id}: {e}")
                        }
                    }
                }
            }
        }
    });
}

/// Average utilisation of `metric` for `app` over the given `window`,
/// as a fraction, from the leader's rollup store. The window comes from
/// the app's `[autoscale] evaluation_window`, not a hardcoded default.
///
/// Returns `None` when there's no data. The value is interpreted as a
/// utilisation fraction (0.0–1.0) to compare against the autoscale
/// target; the metric Mayo records must be scaled accordingly.
async fn app_metric_utilisation(
    rollup_store: &tokio::sync::RwLock<crate::mayo::rollup_store::RollupStore>,
    metric: &str,
    app: &str,
    window: Duration,
) -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let window_start = now.saturating_sub(window.as_secs());

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
#[allow(clippy::too_many_arguments)]
pub fn spawn_placement_reconciler(
    node_name: String,
    metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    directory_rx: watch::Receiver<crate::mustard::directory::NodeDirectory>,
    // Fallback: API port relative to the raft port (ports are uniform
    // ACROSS nodes only as offsets — single-host clusters, like the
    // tests, give every node its own port block). Used only while the
    // gossip directory has no advertised endpoint for the leader.
    raft_to_api_offset: i32,
    service_token: Option<String>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
    cluster_http: crate::cluster::ClusterHttp,
    // Where to persist the durable applied-state checkpoint (DEP3). `None`
    // disables persistence (the reconciler is still correct, it just
    // re-derives applied-state from scratch on every restart).
    state_dir: Option<std::path::PathBuf>,
) {
    tokio::spawn(async move {
        let client = cluster_http.client().clone();
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        let checkpoint_path = state_dir
            .as_deref()
            .map(crate::cluster::applied::checkpoint_path);
        // (name, namespace) → serialized assignment we last SUCCESSFULLY
        // applied. Seeded from the durable checkpoint so a restart doesn't
        // redeploy work that already converged (DEP3).
        let mut applied: BTreeMap<(String, String), String> = checkpoint_path
            .as_deref()
            .map(crate::cluster::applied::load)
            .unwrap_or_default();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }

            // Leader's advertised API address, resolved from Raft metrics
            // (voters) or the gossip directory (everyone — this is what
            // lets a node OUTSIDE the council keep converging, H1/CP1).
            // The reporting offset is passed as 0 because only the API
            // address matters here.
            let leader_url = {
                let metrics = metrics_rx.borrow();
                let directory = directory_rx.borrow();
                crate::cluster::directory::resolve_leader(
                    &metrics,
                    &directory,
                    raft_to_api_offset,
                    0,
                )
                .and_then(|view| view.api_address)
                .map(|address| cluster_http.url(&address.to_string(), ""))
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
            let mut changed = false;
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

                // Drive the deploy and wait for its TERMINAL outcome
                // (DEP3): the placement is applied only when the deploy
                // reports `Complete`, not when the command is queued. A
                // failed deploy leaves `applied` untouched, so the next
                // tick retries it.
                let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
                if cmd_tx
                    .send(AgentCommand::Deploy {
                        config,
                        events: event_tx,
                    })
                    .await
                    .is_err()
                {
                    continue; // agent gone; retry next tick
                }
                if deploy_succeeded(event_rx).await {
                    applied.insert(key, fingerprint);
                    changed = true;
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
                changed = true;
            }

            // Persist the durable checkpoint whenever the applied set moved,
            // so a restart resumes from the converged state (DEP3).
            if changed && let Some(path) = &checkpoint_path {
                crate::cluster::applied::save(path, &applied);
            }
        }
    });
}

/// Drain a deploy's event stream and report whether it reached `Complete`.
///
/// Returns `false` if the deploy emitted `Error` or the channel closed
/// without a terminal event (the agent dropped it), so the caller retries.
async fn deploy_succeeded(mut events: mpsc::Receiver<ApplyEvent>) -> bool {
    while let Some(event) = events.recv().await {
        match event {
            ApplyEvent::Complete { .. } => return true,
            ApplyEvent::Error { .. } => return false,
            _ => {}
        }
    }
    false
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

        let window = Duration::from_secs(300);
        let value = app_metric_utilisation(&store, "cpu", "web", window).await;
        assert!(
            value.is_some_and(|v| (v - 0.8).abs() < 1e-9),
            "expected mean utilisation 0.8, got {value:?}"
        );

        // Unknown app → no data.
        assert!(
            app_metric_utilisation(&store, "cpu", "other", window)
                .await
                .is_none()
        );
    }

    // -- plan_scheduling_pass (CP8 reservation cache, daemon, quotas) ---------

    use crate::council::types::DesiredState;
    use crate::meat::quota::{NamespaceQuota, QuotaLedger};
    use crate::meat::types::AppId;

    fn sched_node(name: &str, cpu: u64, labels: BTreeMap<String, String>) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            running_apps: Default::default(),
            uptime_secs: 86400,
            cached_images: Default::default(),
        }
    }

    fn app_spec(cpu_request: u64, replicas: u32) -> AppSpec {
        let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
        spec.replicas = Replicas::Fixed(replicas);
        spec.cpu = Some(crate::config::types::ResourceRange {
            request: cpu_request,
            limit: cpu_request,
        });
        spec
    }

    /// CP8: two apps that together exceed one node's headroom must NOT both
    /// land on it. The single shared reservation cache makes the second app
    /// see the first app's footprint.
    #[test]
    fn two_apps_do_not_double_book_one_node() {
        // One node with room for exactly one 600m replica (1000m total).
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("solo", 1000, BTreeMap::new()));

        let mut desired = DesiredState::default();
        let a = AppId::new("a", "prod");
        let b = AppId::new("b", "prod");
        desired.apps.insert(a.clone(), app_spec(600, 1));
        desired.apps.insert(b.clone(), app_spec(600, 1));

        let alive = HashSet::from([NodeId::new("solo")]);
        let mut quotas = QuotaLedger::default();
        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);

        // App "a" fits; app "b" cannot (600 + 600 > 1000) — exactly one app
        // is placed, the other is refused rather than double-booked.
        assert_eq!(
            decisions.len(),
            1,
            "second app must not double-book: {decisions:?}"
        );
        assert_eq!(decisions[0].app_id, a);
    }

    /// A cordoned (upgrade) node receives nothing.
    #[test]
    fn cordoned_node_receives_no_placement() {
        let mut cache = ClusterStateCache::new();
        let mut cordoned = sched_node("up", 4000, BTreeMap::new());
        cordoned.ready = false; // apply_upgrade_cordon would set this
        cache.set_node(cordoned);

        let mut desired = DesiredState::default();
        let a = AppId::new("a", "prod");
        desired.apps.insert(a.clone(), app_spec(100, 1));

        let alive = HashSet::from([NodeId::new("up")]);
        let mut quotas = QuotaLedger::default();
        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);
        assert!(
            decisions.is_empty(),
            "a cordoned node must not receive placements: {decisions:?}"
        );
    }

    /// Daemon convergence: a daemon app fans out to every eligible node, so
    /// adding a node grows the placement on the next pass.
    #[test]
    fn daemon_app_gains_a_placement_when_a_node_joins() {
        let mut desired = DesiredState::default();
        let a = AppId::new("mon", "system");
        let mut spec = app_spec(100, 1);
        spec.replicas = Replicas::DaemonSet;
        desired.apps.insert(a.clone(), spec);

        // First pass: two nodes.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(decisions[0].placements.len(), 2);
        // Record the placement as committed.
        desired
            .scheduling
            .insert(a.clone(), decisions[0].placements.clone());

        // A node joins: the daemon must gain a third instance.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n3", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2"), NodeId::new("n3")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(
            decisions[0].placements.len(),
            3,
            "daemon should gain a placement on the new node"
        );
    }

    /// A placement whose node has left is stale, so the app is re-planned.
    #[test]
    fn departed_node_placement_is_replanned() {
        let mut desired = DesiredState::default();
        let a = AppId::new("web", "prod");
        desired.apps.insert(a.clone(), app_spec(100, 1));
        // Committed placement points at a node that is no longer alive.
        desired.scheduling.insert(
            a.clone(),
            vec![crate::meat::types::Placement {
                node_id: NodeId::new("gone"),
                resources: Resources::new(100, 0, 0),
            }],
        );

        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("live", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("live")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(decisions.len(), 1, "departed placement must be re-planned");
        assert_eq!(decisions[0].placements[0].node_id, NodeId::new("live"));
    }

    /// Quota rejection: a namespace over its CPU budget gets its app refused.
    #[test]
    fn quota_over_budget_app_is_rejected() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("big", 10000, BTreeMap::new()));

        let mut desired = DesiredState::default();
        let a = AppId::new("greedy", "prod");
        desired.apps.insert(a.clone(), app_spec(800, 2)); // 1600m requested

        let quota = NamespaceQuota {
            namespace: "prod".to_string(),
            max_cpu_millicores: Some(1000),
            max_memory_bytes: None,
            max_gpus: None,
            max_apps: None,
            max_replicas: None,
        };
        let mut quotas = QuotaLedger::new(std::collections::HashMap::from([(
            "prod".to_string(),
            quota,
        )]));

        let alive = HashSet::from([NodeId::new("big")]);
        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);
        assert!(
            decisions.is_empty(),
            "an app over its namespace quota must be refused: {decisions:?}"
        );
    }

    /// The T6 handoff: a quota built from *desired-state namespaces*
    /// (not a hand-injected table) rejects an over-budget app on the
    /// apply path. This is what `ledger_from_namespaces` at
    /// `orchestrate.rs:150` lights up.
    #[test]
    fn desired_state_namespace_quota_rejects_over_budget_app() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("big", 10000, BTreeMap::new()));

        let mut desired = DesiredState::default();
        desired.namespaces.insert(
            "prod".to_string(),
            crate::config::NamespaceSpec {
                cpu: Some("1000m".to_string()),
                memory: None,
                gpu: None,
                max_apps: None,
                max_replicas: None,
            },
        );
        let a = AppId::new("greedy", "prod");
        desired.apps.insert(a.clone(), app_spec(800, 2)); // 1600m > 1000m

        let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);
        let alive = HashSet::from([NodeId::new("big")]);
        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);
        assert!(
            decisions.is_empty(),
            "namespace budget from desired state must reject the app: {decisions:?}"
        );
    }

    /// A namespace with headroom admits the app.
    #[test]
    fn desired_state_namespace_quota_admits_app_that_fits() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("big", 10000, BTreeMap::new()));

        let mut desired = DesiredState::default();
        desired.namespaces.insert(
            "prod".to_string(),
            crate::config::NamespaceSpec {
                cpu: Some("2000m".to_string()),
                memory: None,
                gpu: None,
                max_apps: None,
                max_replicas: None,
            },
        );
        let a = AppId::new("modest", "prod");
        desired.apps.insert(a.clone(), app_spec(400, 2)); // 800m < 2000m

        let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);
        let alive = HashSet::from([NodeId::new("big")]);
        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);
        assert_eq!(
            decisions.len(),
            1,
            "an app within its namespace budget must be admitted"
        );
    }
}

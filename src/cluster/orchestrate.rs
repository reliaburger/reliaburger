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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bun::agent::{AgentCommand, ApplyEvent};
use crate::cluster::applied::{AppliedMap, AssignmentState};
use crate::config::app::AppSpec;
use crate::config::{Config, Replicas};
use crate::council::node::CouncilNode;
use crate::council::types::{CouncilNodeInfo, CouncilResponse, RaftRequest};
use crate::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};
use crate::meat::types::{NodeId, Resources};
use crate::mustard::membership::MembershipSnapshot;
use crate::mustard::state::NodeState;
use crate::reporting::aggregator::AggregatedState;

/// How often the leader re-evaluates scheduling and nodes poll their
/// assignments.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);
const RECONCILE_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How many retirements one reconcile cycle has in flight at once.
const MAX_CONCURRENT_RETIREMENTS: usize = 4;

/// The deadline for one retirement: queueing behind other agent commands,
/// then the longest a confirmed stop can take.
fn retire_timeout(io_timeout: Duration, stop_confirmation_timeout: Duration) -> Duration {
    io_timeout + crate::bun::agent::stop_completion_bound(stop_confirmation_timeout)
}

/// The longest one owner may take, once a test lease is released, to confirm
/// its share of the cleanup: waiting for its next placement poll, one whole
/// retirement, then its acknowledgement to the leader. Owners retire side by
/// side, so this also bounds the whole release. A retirement never waits for
/// a deploy: a reconcile cycle retires before it deploys, and keeps polling
/// and retiring while it waits on a deploy. It can still queue behind the
/// same owner's previous retirement batch, if that batch is still running.
pub fn lease_retirement_bound(stop_confirmation_timeout: Duration) -> Duration {
    RECONCILE_INTERVAL
        + retire_timeout(RECONCILE_IO_TIMEOUT, stop_confirmation_timeout)
        + RECONCILE_IO_TIMEOUT
}

/// The leader's latest reading of the endpoint withdrawal ledger, exported as
/// Mayo metrics by Bun's collection loop. Followers report zero: only the
/// leader judges the replicated ledger.
#[derive(Debug, Default)]
pub struct WithdrawalLedgerGauge {
    occupancy_permille: std::sync::atomic::AtomicU64,
    pending_generations: std::sync::atomic::AtomicU64,
}

impl WithdrawalLedgerGauge {
    const fn new() -> Self {
        Self {
            occupancy_permille: std::sync::atomic::AtomicU64::new(0),
            pending_generations: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Record the leader's view of the ledger.
    pub fn record(&self, withdrawals: &crate::onion::withdrawal::EndpointWithdrawals) {
        use std::sync::atomic::Ordering::Relaxed;
        let permille = (withdrawals.occupancy() * 1000.0).round() as u64;
        self.occupancy_permille.store(permille, Relaxed);
        self.pending_generations
            .store(withdrawals.pending.len() as u64, Relaxed);
    }

    /// Forget the reading once this node stops leading.
    pub fn clear(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.occupancy_permille.store(0, Relaxed);
        self.pending_generations.store(0, Relaxed);
    }

    /// Samples for Mayo: occupancy as a 0–1 ratio of the tightest bound, and
    /// the number of retained generations.
    pub fn samples(&self) -> [(&'static str, f64); 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            (
                "discovery_withdrawal_ledger_occupancy_ratio",
                self.occupancy_permille.load(Relaxed) as f64 / 1000.0,
            ),
            (
                "discovery_withdrawal_pending_generations",
                self.pending_generations.load(Relaxed) as f64,
            ),
        ]
    }
}

static WITHDRAWAL_LEDGER: WithdrawalLedgerGauge = WithdrawalLedgerGauge::new();

/// The process-wide gauge the leader loop updates.
pub fn withdrawal_ledger_gauge() -> &'static WithdrawalLedgerGauge {
    &WITHDRAWAL_LEDGER
}

/// Ledger occupancy at which the leader starts warning. Publication itself
/// only stops at 100%, so this leaves room to decommission a lost node.
const WITHDRAWAL_BACKLOG_WARNING: f64 = 0.75;

/// Readiness subsystem the leader degrades while the withdrawal ledger is
/// close to refusing catalogue updates. It is informational: scheduling
/// continues, but operators see which nodes owe receipts.
pub const WITHDRAWAL_BACKLOG_SUBSYSTEM: &str = "discovery:withdrawal-backlog";

/// Operator warning once the withdrawal ledger nears its bound, naming the
/// consumers that owe receipts and whether gossip still sees them alive.
pub(crate) fn withdrawal_backlog_warning(
    withdrawals: &crate::onion::withdrawal::EndpointWithdrawals,
    alive: &HashSet<&str>,
) -> Option<String> {
    let occupancy = withdrawals.occupancy();
    if occupancy < WITHDRAWAL_BACKLOG_WARNING {
        return None;
    }
    let mut owing: Vec<_> = withdrawals.owed_by_consumer().into_iter().collect();
    // Most-owing first; a lost node usually owes every retained generation.
    owing.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let owing = owing
        .iter()
        .map(|(node, generations)| {
            let state = if alive.contains(node) {
                "alive"
            } else {
                "not alive"
            };
            format!("{node} ({generations} generations, {state})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "endpoint withdrawal ledger is {:.0}% full; catalogue updates stop at 100%. \
         Receipts owed by: {owing}. If a node is permanently gone, run \
         `relish decommission-node <node> --workloads-stopped --reason <why>`",
        occupancy * 100.0
    ))
}

/// One app assigned to a node, as served by `/v1/placements/{node}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAssignment {
    pub name: String,
    pub namespace: String,
    /// The cluster-wide ordinals of the replicas assigned to the node,
    /// lowest first. Their count is the node's share of the app.
    pub ordinals: Vec<u32>,
    pub spec: AppSpec,
}

/// An ingress route distributed to every node, including nodes without replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressAssignment {
    /// Application name.
    pub name: String,
    /// Application namespace.
    pub namespace: String,
    /// Desired ingress configuration.
    pub config: crate::config::app::IngressSpec,
}

/// Every ingress route the cluster serves, for every node's routing table.
///
/// A stopped app keeps its spec, so the next apply restores it, but serves no
/// traffic. It has no route, and the proxy answers 404 for its host.
pub fn cluster_ingress(desired: &crate::council::types::DesiredState) -> Vec<IngressAssignment> {
    desired
        .apps
        .iter()
        .filter(|(id, _)| !desired.stopped_apps.contains(*id))
        .filter_map(|(id, spec)| {
            spec.ingress.clone().map(|config| IngressAssignment {
                name: id.name.clone(),
                namespace: id.namespace.clone(),
                config,
            })
        })
        .collect()
}

/// An exact lease generation whose runtime ownership must retire on one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRetirement {
    /// Immutable lease identifier; namespace reuse cannot acknowledge another lease.
    pub lease_id: String,
    /// Application and node whose absence the reconciler must establish.
    pub placement: crate::testkit::lease::LeasedPlacement,
}

/// The full assignment list for a node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeAssignments {
    pub apps: Vec<NodeAssignment>,
    /// Confirmed-cleanup instructions, including nodes absent from current placement.
    pub retirements: Vec<LeaseRetirement>,
    /// Committed publication generation shared by this catalogue and its instructions.
    pub endpoint_generation: u64,
    /// Current cluster-wide catalogue, required even when explicitly empty.
    pub endpoint_catalog: crate::onion::catalog::EndpointCatalog,
    /// This consumer's original routes awaiting confirmed local withdrawal.
    pub endpoint_withdrawals: Vec<crate::onion::withdrawal::EndpointWithdrawalInstruction>,
    /// Cluster-wide ingress routes, independent of local placements.
    #[serde(default)]
    pub ingress: Vec<IngressAssignment>,
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
    dns_required: bool,
    reconstruction_config: crate::config::node::ReconstructionSection,
    readiness: Option<crate::bun::readiness::ReadinessTracker>,
    shutdown: CancellationToken,
) -> super::capacity::CapacityAdmission {
    use crate::reconstruction::controller::ReconstructionController;
    use crate::reconstruction::types::ReconstructionPhase;

    let (admission, mut capacity_requests) = super::capacity::admission_channel();
    tokio::spawn(async move {
        let mut reconstruction = ReconstructionController::new(reconstruction_config);
        let mut was_leader = false;
        let mut backlog_warning: Option<String> = None;
        if let Some(readiness) = &readiness {
            readiness
                .register(WITHDRAWAL_BACKLOG_SUBSYSTEM, false)
                .await;
            readiness.ready(WITHDRAWAL_BACKLOG_SUBSYSTEM).await;
        }
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            let capacity_request = tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => None,
                Some(request) = capacity_requests.recv() => Some(request),
            };

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
                // Only the leader judges the replicated ledger.
                withdrawal_ledger_gauge().clear();
                if backlog_warning.take().is_some()
                    && let Some(readiness) = &readiness
                {
                    readiness.ready(WITHDRAWAL_BACKLOG_SUBSYSTEM).await;
                }
                continue;
            }

            let desired = council.desired_state().await;
            let mut members = membership_rx.borrow().clone();
            members.retain(|member| {
                !desired
                    .security_state
                    .crl
                    .retired_nodes
                    .contains_key(&member.node_id.0)
            });
            let reports = aggregated_rx.borrow().clone();

            let alive_names: HashSet<&str> = members
                .iter()
                .filter(|member| member.state == NodeState::Alive)
                .map(|member| member.node_id.0.as_str())
                .collect();
            // A stopped node can never confirm a withdrawal. Once its view
            // lease has certainly run out, stop waiting for it (Z6.7).
            let discharged = super::consumer::discharge_lapsed_consumers(
                &council,
                &alive_names,
                crate::onion::lease::CONSUMER_DISCHARGE_AFTER,
            )
            .await;
            let desired = if discharged.is_empty() {
                desired
            } else {
                council.desired_state().await
            };
            withdrawal_ledger_gauge().record(&desired.endpoint_withdrawals);
            let warning = withdrawal_backlog_warning(&desired.endpoint_withdrawals, &alive_names);
            if warning != backlog_warning {
                if let Some(readiness) = &readiness {
                    match &warning {
                        Some(message) => {
                            readiness
                                .degraded(WITHDRAWAL_BACKLOG_SUBSYSTEM, message.clone())
                                .await
                        }
                        None => readiness.ready(WITHDRAWAL_BACKLOG_SUBSYSTEM).await,
                    }
                }
                if let Some(message) = &warning {
                    eprintln!("scheduler: {message}");
                }
                backlog_warning = warning;
            }

            // Publish the cluster endpoint catalogue every tick the backends
            // change (12b.4). This runs before the learning-period gate below:
            // cross-node resolution shouldn't wait for a fresh leader to finish
            // reconstructing placements — the reports already say what's
            // running where, and a stale catalogue is worse than an early one.
            let catalog = match build_endpoint_catalog(&members, &reports, &desired) {
                Ok(catalog) => Some(catalog),
                Err(error) => {
                    eprintln!("scheduler: failed to allocate endpoint catalogue: {error}");
                    None
                }
            };
            // The state machine plans with these exact inputs, so a full
            // ledger would only commit another refusal every tick.
            if let Some(catalog) = catalog
                && desired.endpoint_catalog != catalog
                && !matches!(
                    desired.endpoint_withdrawals.plan_publication(
                        &desired.endpoint_catalog,
                        &catalog,
                        &desired.endpoint_consumers,
                    ),
                    Err(crate::onion::withdrawal::WithdrawalError::CapacityReached)
                )
            {
                match council
                    .write(RaftRequest::PublishEndpoints {
                        expected_generation: desired.endpoint_withdrawals.generation,
                        catalog: Box::new(catalog),
                    })
                    .await
                {
                    // A committed request can still be refused by the state
                    // machine. Only committed catalogue state suppresses retry.
                    Ok(CouncilResponse::Applied { .. }) => {}
                    Ok(response) => {
                        eprintln!("scheduler: endpoint catalogue was not applied: {response:?}");
                    }
                    Err(e) => eprintln!("scheduler: failed to publish endpoint catalogue: {e}"),
                }
            }

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

            let unheard = unheard_nodes(&alive, &reports);
            let suspect = nodes_in_state(&members, NodeState::Suspect);
            let PassPlan {
                decisions,
                quota_blocked,
            } = plan_pass(
                &mut cache,
                &desired,
                &alive,
                &suspect,
                &mut quotas,
                dns_required,
                &unheard,
            );

            if let Some(request) = capacity_request {
                use super::capacity::CapacityAdmissionError;
                use crate::meat::scheduler::{ScheduleError, Scheduler};

                let ready = !has_stale_member(&members, &reports)
                    && members
                        .iter()
                        .all(|member| member.state == NodeState::Alive)
                    && cache.node_count() == alive.len()
                    && cache.nodes().all(|node| {
                        node.ready
                            && (!dns_required || node.capabilities.dns.can_resolve_internal())
                    });
                let result = if !ready {
                    Err(CapacityAdmissionError::Unavailable(
                        "every member needs fresh, ready placement evidence".into(),
                    ))
                } else if desired.apps.contains_key(&request.app_id) {
                    Err(CapacityAdmissionError::Rejected(
                        ScheduleError::InvalidSpec {
                            reason: "capacity admission requires a new app".into(),
                        },
                    ))
                } else if let Err(error) = quotas.try_admit(
                    &request.app_id.namespace,
                    &scheduler_resources(&request.spec),
                    1,
                    true,
                ) {
                    Err(CapacityAdmissionError::Rejected(
                        ScheduleError::QuotaExceeded {
                            namespace: request.app_id.namespace.clone(),
                            detail: error.to_string(),
                        },
                    ))
                } else {
                    Scheduler::new(cache)
                        .with_dns_required(dns_required)
                        .schedule_app(&request.app_id, &request.spec)
                        .map(|_| ())
                        .map_err(CapacityAdmissionError::Rejected)
                };
                let _ = request.response.send(result);
                continue;
            }

            for decision in decisions {
                // Revalidate against the LATEST membership before the async
                // Raft write: a node that died between planning and commit
                // (or between two commits in this pass) must not receive the
                // placement. `members`/`reports` were snapshotted at the top
                // of the tick; membership can move under a slow write. A
                // suspect node still holds the placements it kept.
                let live: HashSet<NodeId> = membership_rx
                    .borrow()
                    .iter()
                    .filter(|m| matches!(m.state, NodeState::Alive | NodeState::Suspect))
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
            // Why an app isn't placed is durable council state, so any node
            // can answer `relish status`. Written only when it changes.
            if let Some(update) = quota_blocked_update(&desired.quota_blocked, &quota_blocked)
                && let Err(e) = council.write(update).await
            {
                eprintln!("scheduler: failed to record quota-blocked apps: {e}");
            }
        }
    });
    admission
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
#[cfg(test)]
fn plan_scheduling_pass(
    cache: &mut ClusterStateCache,
    desired: &crate::council::types::DesiredState,
    alive: &HashSet<NodeId>,
    quotas: &mut crate::meat::quota::QuotaLedger,
) -> Vec<crate::meat::types::SchedulingDecision> {
    plan_scheduling_pass_with_dns(
        cache,
        desired,
        alive,
        &HashSet::new(),
        quotas,
        false,
        &HashSet::new(),
    )
}

/// The placements a pass decided on, for tests that only look at those.
#[cfg(test)]
fn plan_scheduling_pass_with_dns(
    cache: &mut ClusterStateCache,
    desired: &crate::council::types::DesiredState,
    alive: &HashSet<NodeId>,
    suspect: &HashSet<NodeId>,
    quotas: &mut crate::meat::quota::QuotaLedger,
    dns_required: bool,
    unheard: &HashSet<NodeId>,
) -> Vec<crate::meat::types::SchedulingDecision> {
    plan_pass(
        cache,
        desired,
        alive,
        suspect,
        quotas,
        dns_required,
        unheard,
    )
    .decisions
}

/// What one scheduling pass decided.
#[derive(Debug, Default)]
struct PassPlan {
    /// Placements to commit.
    decisions: Vec<crate::meat::types::SchedulingDecision>,
    /// Apps the pass refused to place because their namespace quota has no
    /// room, with the reason. Ordered so the Raft write is deterministic.
    quota_blocked: BTreeMap<crate::meat::types::AppId, crate::meat::quota::QuotaError>,
}

/// The `QuotaBlocked` write that brings the recorded reasons in line with
/// what this pass found, or `None` when they already match.
///
/// The leader plans a pass every tick, and an over-quota app stays blocked
/// for as long as nobody changes the quota or the app. Writing the same set
/// every tick would grow the Raft log for nothing, so only a change is
/// proposed.
fn quota_blocked_update(
    recorded: &HashMap<crate::meat::types::AppId, crate::meat::quota::QuotaError>,
    planned: &BTreeMap<crate::meat::types::AppId, crate::meat::quota::QuotaError>,
) -> Option<RaftRequest> {
    let unchanged = recorded.len() == planned.len()
        && planned
            .iter()
            .all(|(app_id, reason)| recorded.get(app_id) == Some(reason));
    if unchanged {
        return None;
    }
    Some(RaftRequest::QuotaBlocked {
        blocked: planned
            .iter()
            .map(|(app_id, reason)| (app_id.clone(), reason.clone()))
            .collect(),
    })
}

/// Plan a pass with the cluster's configured DNS requirement.
///
/// The requirement is an explicit configuration input. Deriving it from live
/// capability reports creates a fail-open edge: if every DNS lease expires,
/// absence would look exactly like an intentionally disabled resolver.
fn plan_pass(
    cache: &mut ClusterStateCache,
    desired: &crate::council::types::DesiredState,
    alive: &HashSet<NodeId>,
    suspect: &HashSet<NodeId>,
    quotas: &mut crate::meat::quota::QuotaLedger,
    dns_required: bool,
    unheard: &HashSet<NodeId>,
) -> PassPlan {
    use crate::meat::scheduler::Scheduler;

    for node_id in cache.node_ids() {
        if desired
            .security_state
            .crl
            .retired_nodes
            .contains_key(&node_id.0)
            && let Some(mut node) = cache.get_node(&node_id).cloned()
        {
            node.ready = false;
            cache.set_node(node);
        }
    }
    // Reports can lag committed assignments by several ticks. Reconstruct
    // missing reservations before admitting any app, including when all of
    // an earlier app's placements are already converged.
    for (app_id, placements) in &desired.scheduling {
        let mut counts = HashMap::<NodeId, u32>::new();
        for placement in placements {
            let committed = counts.entry(placement.node_id.clone()).or_default();
            *committed += 1;
            let reported = cache
                .get_node(&placement.node_id)
                .map_or(0, |node| node.replicas_of(app_id));
            if *committed > reported {
                cache.reserve(&placement.node_id, app_id, &placement.resources);
            }
        }
    }
    let retired: HashSet<NodeId> = desired
        .security_state
        .crl
        .retired_nodes
        .keys()
        .map(NodeId::new)
        .collect();
    let liveness = Liveness {
        alive,
        suspect,
        unheard,
        retired: &retired,
        dns_required,
    };
    let mut decisions = Vec::new();
    let mut quota_blocked = BTreeMap::new();
    // A stable order so a pass is deterministic (HashMap iteration isn't).
    let mut app_ids: Vec<_> = desired.apps.keys().cloned().collect();
    app_ids.sort_by_key(|a| a.to_string());

    // Precompute each app's target replica count and whether it is already
    // converged, up front. A placement is only OK if it targets a node still
    // alive, ready and capable of enforcing the spec — capability loss is a
    // state change that forces the same re-plan as node loss or a cordon.
    let mut planned: Vec<(
        &crate::meat::types::AppId,
        &AppSpec,
        Option<u32>,
        usize,
        bool,
    )> = Vec::with_capacity(app_ids.len());
    for app_id in &app_ids {
        let Some(spec) = desired.apps.get(app_id) else {
            continue;
        };
        // `relish stop` pins an app at zero until it is applied again.
        let override_replicas = if desired.stopped_apps.contains(app_id) {
            Some(0)
        } else {
            desired
                .autoscale_overrides
                .iter()
                .find(|(k, _)| k == &app_id.to_string())
                .map(|(_, n)| *n)
        };
        // A daemon set targets every *eligible* node, so its convergence count
        // is the eligible-node count, not every alive node (M25).
        let want = if override_replicas.is_none() && matches!(spec.replicas, Replicas::DaemonSet) {
            daemon_eligible_count(
                cache,
                app_id,
                spec,
                dns_required,
                &committed_footprints(desired, app_id),
            )
        } else {
            effective_replicas(spec, override_replicas, alive.len())
        };
        let requested = scheduler_resources(spec);
        let converged = desired
            .scheduling
            .get(app_id)
            .map(|placements| {
                placements.len() == want
                    && placements
                        .iter()
                        .all(|p| p.resources == requested && liveness.keeps(p, spec, cache))
            })
            .unwrap_or(false);
        planned.push((app_id, spec, override_replicas, want, converged));
    }

    // Seed the quota ledger with every already-converged app's footprint
    // BEFORE admitting any new app. Converged apps `continue` below and never
    // reach `try_admit`; without this seeding they contribute nothing to their
    // namespace's usage, so a later new app is admitted against a clean slate
    // and the budget is silently busted once the earlier apps converge.
    if !quotas.is_empty() {
        for (app_id, spec, _, want, converged) in &planned {
            if *converged {
                quotas.charge_committed(
                    &app_id.namespace,
                    &scheduler_resources(spec),
                    *want as u32,
                );
            }
        }
    }

    for (app_id, spec, override_replicas, want, converged) in planned {
        if converged {
            continue;
        }

        // Quota admission (cumulative within the pass, on top of the seeded
        // committed usage). A rejection is returned with the plan, and the
        // leader records it in council state, so the app isn't left pending
        // without an explanation anyone can read.
        let per_replica = scheduler_resources(spec);
        let is_new_app = !desired.scheduling.contains_key(app_id);
        if !quotas.is_empty()
            && let Err(e) =
                quotas.try_admit(&app_id.namespace, &per_replica, want as u32, is_new_app)
        {
            // Log only news, not the same rejection every tick.
            if desired.quota_blocked.get(app_id) != Some(&e) {
                eprintln!("scheduler: quota rejects {app_id}: {e}");
            }
            quota_blocked.insert(app_id.clone(), e);
            continue;
        }

        // Feed the scheduler the effective replica count. The scheduler
        // reserves into the SHARED cache, so the next app sees this app's
        // footprint.
        let mut effective_spec = spec.clone();
        if let Some(n) = override_replicas {
            effective_spec.replicas = Replicas::Fixed(n);
        }
        // A fixed-size app keeps the placements that still hold and only
        // places the rest. Losing one node of three must not move the
        // replicas on the other two: they're serving, and replacing them
        // would restart them for nothing. A scale-down keeps the lowest
        // ordinals, so it retires the highest.
        let previous = desired
            .scheduling
            .get(app_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut kept: Vec<crate::meat::types::Placement> = match effective_spec.replicas {
            Replicas::Fixed(_) => {
                let mut holding: Vec<_> = previous
                    .iter()
                    .filter(|p| liveness.keeps(p, spec, cache))
                    .cloned()
                    .collect();
                holding.sort_by_key(|p| p.ordinal);
                holding.truncate(want);
                holding
            }
            Replicas::DaemonSet => Vec::new(),
        };
        // A kept replica whose request changed is resized where it runs:
        // its node trades the footprint it was committed with for the new
        // one, and the decision records the new one.
        for placement in &mut kept {
            if placement.resources != per_replica {
                cache.release(&placement.node_id, app_id, &placement.resources);
                cache.reserve(&placement.node_id, app_id, &per_replica);
                placement.resources = per_replica;
            }
        }
        // An app with no placement left (it was stopped, or is starting
        // again) goes back to the nodes that hold its managed volumes.
        if kept.is_empty() && matches!(effective_spec.replicas, Replicas::Fixed(_)) {
            let home = VolumeHome {
                cache,
                alive,
                suspect,
                unheard,
                retired: &retired,
                dns_required,
            };
            match home.reserve(app_id, spec, desired.last_placed_nodes.get(app_id), want) {
                HomeOutcome::Placed(placements) => kept = placements,
                HomeOutcome::Wait { node, reason } => {
                    eprintln!(
                        "scheduler: {app_id} waits for {node}, which holds its volumes: {reason}"
                    );
                    continue;
                }
            }
        }
        if !kept.is_empty() {
            let missing = want - kept.len();
            if missing == 0 {
                decisions.push(crate::meat::types::SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: number_placements(previous, kept, Vec::new()),
                });
                continue;
            }
            effective_spec.replicas = Replicas::Fixed(missing as u32);
        }
        // Spread the rest against what's kept, not against what the nodes
        // last reported: a report can lag a new leader, or still list a
        // replica on its way out.
        if matches!(effective_spec.replicas, Replicas::Fixed(_)) {
            count_kept_replicas(cache, app_id, &kept);
        }
        // The scheduler owns its cache, so hand it the shared one and take
        // it back afterwards (Rust move semantics — no shared &mut alias).
        // Snapshot first: a partially-placed fixed-replica app reserves some
        // nodes and then errors (M25), and writing the scheduler's mutated
        // cache back unconditionally would leak those phantom reservations
        // into the shared pass, wrongly starving later apps. On error we
        // restore the pre-call cache; only a successful placement's
        // reservations are kept.
        let snapshot = (*cache).clone();
        let mut scheduler = Scheduler::new(std::mem::take(cache))
            .with_dns_required(dns_required)
            .with_daemon_credit(committed_footprints(desired, app_id));
        let result = scheduler.schedule_app(app_id, &effective_spec);
        match result {
            Ok(mut decision) => {
                *cache = scheduler.cluster;
                let added = std::mem::take(&mut decision.placements);
                decision.placements = number_placements(previous, kept, added);
                decisions.push(decision);
            }
            Err(e) => {
                *cache = snapshot;
                eprintln!("scheduler: cannot place {app_id}: {e}");
            }
        }
    }
    PassPlan {
        decisions,
        quota_blocked,
    }
}

/// Give an app's placements their cluster-wide ordinals, distinct within the
/// app (#398), lowest first.
///
/// `kept` placements already hold theirs. Each `added` one takes the ordinal
/// its node held in `previous` while that's still free, which keeps a daemon
/// set's replica on a node numbered as it was when the set is re-placed
/// whole. Any other takes the lowest free ordinal: the one a lost replica
/// gave up, or the next one up on a scale-up.
fn number_placements(
    previous: &[crate::meat::types::Placement],
    kept: Vec<crate::meat::types::Placement>,
    added: Vec<crate::meat::types::Placement>,
) -> Vec<crate::meat::types::Placement> {
    let mut taken: HashSet<u32> = kept.iter().map(|p| p.ordinal).collect();
    let mut placements = kept;
    let mut unnumbered = Vec::new();
    for mut placement in added {
        let held = previous
            .iter()
            .find(|p| p.node_id == placement.node_id && !taken.contains(&p.ordinal));
        match held {
            Some(held) => {
                placement.ordinal = held.ordinal;
                taken.insert(held.ordinal);
                placements.push(placement);
            }
            None => unnumbered.push(placement),
        }
    }
    let mut free = (0u32..).filter(|ordinal| !taken.contains(ordinal));
    for mut placement in unnumbered {
        // `free` is endless: there are fewer placements than u32 ordinals.
        placement.ordinal = free.next().unwrap_or(u32::MAX);
        placements.push(placement);
    }
    placements.sort_by_key(|p| p.ordinal);
    placements
}

/// Record in `cache` exactly the replicas of `app_id` that `kept` places on
/// each node, so the scheduler spreads what it adds around them.
fn count_kept_replicas(
    cache: &mut ClusterStateCache,
    app_id: &crate::meat::types::AppId,
    kept: &[crate::meat::types::Placement],
) {
    for node_id in cache.node_ids() {
        let count = kept.iter().filter(|p| p.node_id == node_id).count();
        cache.set_replicas(&node_id, app_id, u32::try_from(count).unwrap_or(u32::MAX));
    }
}

/// What returning an app to the nodes that hold its volumes came to.
enum HomeOutcome {
    /// Replicas reserved on their volumes' nodes; empty when the app has no
    /// managed volume or none of its nodes can run it any more, so the
    /// scheduler places it freely.
    Placed(Vec<crate::meat::types::Placement>),
    /// A node holding the app's volumes is still in the cluster but can't
    /// take it right now. Placing it elsewhere would start it on an empty
    /// volume, so it waits.
    Wait { node: NodeId, reason: HomeWait },
}

/// Why an app waits for the node that holds its volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeWait {
    /// The node is out of the cluster: suspected, Left, Dead or forgotten by
    /// gossip. That's usually a restart or a reboot, with the data still on
    /// its disk (#423). Only decommissioning it says the data is gone.
    Away,
    /// The node is alive but this leader has no fresh report from it: a new
    /// leader, or a report worker that stalled long enough to go stale.
    Unreported,
    /// The node reported it isn't ready, is cordoned for an upgrade, or
    /// lacks a capability the app needs.
    NotReady,
    /// The node is ready but short of room.
    NoRoom,
}

impl std::fmt::Display for HomeWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HomeWait::Away => "it is out of the cluster; decommission it to move the app",
            HomeWait::Unreported => "it hasn't reported fresh state",
            HomeWait::NotReady => "it isn't ready",
            HomeWait::NoRoom => "it has no room yet",
        })
    }
}

/// The leader's view of the nodes a returning app's volumes live on.
struct VolumeHome<'a> {
    cache: &'a mut ClusterStateCache,
    alive: &'a HashSet<NodeId>,
    suspect: &'a HashSet<NodeId>,
    unheard: &'a HashSet<NodeId>,
    /// Decommissioned nodes, whose volumes the operator has written off.
    retired: &'a HashSet<NodeId>,
    dns_required: bool,
}

impl VolumeHome<'_> {
    /// Reserve up to `want` replicas of `app_id` on the nodes it last ran on,
    /// if it has a managed volume.
    ///
    /// Only the operator releases a home: by decommissioning the node, which
    /// writes off its volumes, or by changing the app's required labels so
    /// the node no longer matches. Anything else makes the app wait, because
    /// its data is still there: the node being out of the cluster (a bun
    /// restart announces Left, a reboot goes Suspect then Dead, #423), a
    /// stale or missing report, not ready, cordoned for an upgrade, or full.
    fn reserve(
        self,
        app_id: &crate::meat::types::AppId,
        spec: &AppSpec,
        last_nodes: Option<&Vec<NodeId>>,
        want: usize,
    ) -> HomeOutcome {
        let Some(last_nodes) = last_nodes.filter(|_| has_managed_volume(spec)) else {
            return HomeOutcome::Placed(Vec::new());
        };
        let resources = scheduler_resources(spec);
        let required = spec
            .placement
            .as_ref()
            .map(|p| crate::meat::scheduler::parse_label_list(&p.required))
            .unwrap_or_default();
        let mut homes = Vec::new();
        // The leader records an app's nodes in ordinal order, so each home
        // gets back the ordinal its replica had (#398).
        for (ordinal, node_id) in (0u32..).zip(last_nodes.iter().take(want)) {
            let wait = |reason| HomeOutcome::Wait {
                node: node_id.clone(),
                reason,
            };
            if self.retired.contains(node_id) {
                continue;
            }
            if self.suspect.contains(node_id) || !self.alive.contains(node_id) {
                return wait(HomeWait::Away);
            }
            let Some(node) = self.cache.get_node(node_id) else {
                return wait(HomeWait::Unreported);
            };
            if !node.matches_labels(&required) {
                continue;
            }
            if self.unheard.contains(node_id) {
                return wait(HomeWait::Unreported);
            }
            if !node_can_run(node, spec, self.dns_required) {
                return wait(HomeWait::NotReady);
            }
            if !node.can_fit(&resources) {
                return wait(HomeWait::NoRoom);
            }
            homes.push(crate::meat::types::Placement {
                node_id: node_id.clone(),
                resources,
                ordinal,
            });
        }
        // Reserve only once every home is known to fit, so a wait leaves
        // no phantom reservation behind for the rest of the pass.
        for placement in &homes {
            self.cache.reserve(&placement.node_id, app_id, &resources);
        }
        HomeOutcome::Placed(homes)
    }
}

/// Whether an existing placement can stay where it is: its node is alive and
/// ready and can enforce what the spec needs.
///
/// A live node that hasn't reported to this leader yet keeps its placements,
/// and so does one in `unheard`, whose state report has arrived but whose
/// readiness or capability evidence hasn't. A new leader hears from nodes
/// over several seconds, one report at a time, and a node whose report went
/// to a council member that just died can take longer still; "not heard from
/// yet" is not evidence of trouble, and moving its replicas would restart
/// healthy workloads. A node that reported and went stale, or reported not
/// ready or not capable, does lose them. (A managed-volume app's placement
/// never gets this far: [`Liveness::keeps`] keeps it.)
fn placement_holds(
    placement: &crate::meat::types::Placement,
    spec: &AppSpec,
    cache: &ClusterStateCache,
    alive: &HashSet<NodeId>,
    suspect: &HashSet<NodeId>,
    unheard: &HashSet<NodeId>,
    dns_required: bool,
) -> bool {
    // Suspicion is gossip's "missed a probe", not "gone": SWIM gives the
    // node its suspicion timeout to refute it. Moving its replicas now would
    // stop healthy ones for a late ack (#346).
    if suspect.contains(&placement.node_id) {
        return true;
    }
    if !alive.contains(&placement.node_id) {
        return false;
    }
    if unheard.contains(&placement.node_id) {
        return true;
    }
    cache
        .get_node(&placement.node_id)
        .is_none_or(|node| node_can_run(node, spec, dns_required))
}

/// What a pass knows about the cluster's members, for deciding which
/// committed placements stay where they are.
struct Liveness<'a> {
    alive: &'a HashSet<NodeId>,
    suspect: &'a HashSet<NodeId>,
    unheard: &'a HashSet<NodeId>,
    /// Decommissioned nodes, whose volumes the operator has written off.
    retired: &'a HashSet<NodeId>,
    dns_required: bool,
}

impl Liveness<'_> {
    /// Whether a committed placement stays on its node this pass.
    ///
    /// A changed hard selector is the one spec change that moves a replica,
    /// volume or not: the operator asked for other nodes. Otherwise a
    /// managed-volume app stays on its home whatever its node is doing
    /// (#423), even when its new request no longer fits there. Any other
    /// replica stays while its node holds it and still has room for the
    /// request it makes now (#434).
    fn keeps(
        &self,
        placement: &crate::meat::types::Placement,
        spec: &AppSpec,
        cache: &ClusterStateCache,
    ) -> bool {
        let required = spec
            .placement
            .as_ref()
            .map(|p| crate::meat::scheduler::parse_label_list(&p.required))
            .unwrap_or_default();
        if cache
            .get_node(&placement.node_id)
            .is_some_and(|node| !node.matches_labels(&required))
        {
            return false;
        }
        if keeps_volume_home(placement, spec, self.retired) {
            return true;
        }
        placement_holds(
            placement,
            spec,
            cache,
            self.alive,
            self.suspect,
            self.unheard,
            self.dns_required,
        ) && admits_in_place(placement, spec, cache)
    }
}

/// Whether `placement`'s node has room for the spec's current request once
/// the footprint the placement was committed with is credited back: a
/// resized replica replaces the old one rather than running beside it. A
/// node the leader has no report for keeps it, as [`placement_holds`] does.
fn admits_in_place(
    placement: &crate::meat::types::Placement,
    spec: &AppSpec,
    cache: &ClusterStateCache,
) -> bool {
    let requested = scheduler_resources(spec);
    if placement.resources == requested {
        return true;
    }
    cache.get_node(&placement.node_id).is_none_or(|node| {
        let others = node.allocated.saturating_sub(&placement.resources);
        node.allocatable.saturating_sub(&others).fits(&requested)
    })
}

/// The footprint each node's committed placement of `app_id` was admitted
/// with. A daemon set credits these back when it is re-planned, so its own
/// running copy doesn't count against the copy that replaces it (#433).
fn committed_footprints(
    desired: &crate::council::types::DesiredState,
    app_id: &crate::meat::types::AppId,
) -> HashMap<NodeId, Resources> {
    desired
        .scheduling
        .get(app_id)
        .map(|placements| {
            placements
                .iter()
                .map(|p| (p.node_id.clone(), p.resources))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a managed-volume app's placement stays put whatever its node is
/// doing: its replacement would start on an empty volume while the data sits
/// on that node. Restarting, rebooting, Dead and not-ready nodes all keep it
/// (#423); only decommissioning the node releases it.
fn keeps_volume_home(
    placement: &crate::meat::types::Placement,
    spec: &AppSpec,
    retired: &HashSet<NodeId>,
) -> bool {
    has_managed_volume(spec) && !retired.contains(&placement.node_id)
}

/// The node holding `app_id`'s managed volume while that node is out of the
/// cluster, so the app waits for it: the first of its placements, or with
/// none, of the nodes it last ran on, that isn't in `live` and hasn't been
/// decommissioned. `None` for an app without a managed volume, a stopped
/// app, or one whose homes are all live.
pub(crate) fn volume_home_away(
    desired: &crate::council::types::DesiredState,
    app_id: &crate::meat::types::AppId,
    spec: &AppSpec,
    live: &HashSet<String>,
) -> Option<NodeId> {
    if desired.stopped_apps.contains(app_id) {
        return None;
    }
    volume_homes(desired, app_id, spec)
        .into_iter()
        .find(|node| !live.contains(&node.0))
}

/// Every node that holds `app_id`'s managed volumes, live or not: its
/// placements, or with none (a stopped app), the nodes it last ran on, less
/// any decommissioned node, in placement order without repeats. Empty for an
/// app without a managed volume.
pub(crate) fn volume_homes(
    desired: &crate::council::types::DesiredState,
    app_id: &crate::meat::types::AppId,
    spec: &AppSpec,
) -> Vec<NodeId> {
    if !has_managed_volume(spec) {
        return Vec::new();
    }
    let placed: Vec<&NodeId> = desired
        .scheduling
        .get(app_id)
        .map(|placements| placements.iter().map(|p| &p.node_id).collect())
        .unwrap_or_default();
    let homes = if placed.is_empty() {
        desired
            .last_placed_nodes
            .get(app_id)
            .map(|nodes| nodes.iter().collect())
            .unwrap_or_default()
    } else {
        placed
    };
    let retired = &desired.security_state.crl.retired_nodes;
    let mut unique: Vec<NodeId> = Vec::new();
    for node in homes {
        if !retired.contains_key(&node.0) && !unique.contains(node) {
            unique.push(node.clone());
        }
    }
    unique
}

/// Whether a fixed-size app keeps state in a managed volume, which lives on
/// the node it runs on. A daemon set runs on every eligible node anyway, so
/// there is nowhere else for its volume to be.
fn has_managed_volume(spec: &AppSpec) -> bool {
    matches!(spec.replicas, Replicas::Fixed(_))
        && spec.volumes.iter().any(|volume| volume.source.is_none())
}

/// Whether a reported node is ready and can enforce what `spec` needs.
fn node_can_run(
    node: &crate::meat::cluster_state::SchedulerNodeState,
    spec: &AppSpec,
    dns_required: bool,
) -> bool {
    let requires_egress = spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty());
    node.ready
        && (!requires_egress || node.capabilities.egress.can_enforce_allowlist())
        && (!dns_required || node.capabilities.dns.can_resolve_internal())
}

/// Whether any member the scheduler plans over has a stale report. Only
/// members count: the aggregator keeps a deadline for every gossip member,
/// including retired ones the scheduler has already filtered out, and a node
/// that never reports again mustn't block capacity admission for good.
fn has_stale_member(members: &[MembershipSnapshot], reports: &AggregatedState) -> bool {
    members
        .iter()
        .any(|member| reports.stale_nodes.contains(&member.node_id))
}

/// The members gossip currently puts in `state`.
fn nodes_in_state(members: &[MembershipSnapshot], state: NodeState) -> HashSet<NodeId> {
    members
        .iter()
        .filter(|member| member.state == state)
        .map(|member| member.node_id.clone())
        .collect()
}

/// Live nodes whose state report is fresh but whose readiness or capability
/// report this leader hasn't received yet. The three travel separately, and a
/// new leader starts with none of them, so for a moment a healthy node looks
/// unready. That's reason enough not to place anything new there, but not to
/// move what it already runs.
fn unheard_nodes(alive: &HashSet<NodeId>, reports: &AggregatedState) -> HashSet<NodeId> {
    alive
        .iter()
        .filter(|node| reports.reports.contains_key(*node))
        .filter(|node| !reports.stale_nodes.contains(*node))
        .filter(|node| {
            !reports.readiness.contains_key(*node) || !reports.capabilities.contains_key(*node)
        })
        .cloned()
        .collect()
}

/// The number of nodes a daemon set of `spec` can currently be placed on
/// (M25). A daemon set targets every *eligible* node, not every alive node, so
/// its convergence must be judged against this — otherwise, whenever any alive
/// node is ineligible (not ready, lacks a required capability, doesn't fit),
/// `placements.len()` never equals `alive.len()` and the leader re-commits an
/// identical `SchedulingDecision` to Raft every tick.
fn daemon_eligible_count(
    cache: &ClusterStateCache,
    app_id: &crate::meat::AppId,
    spec: &AppSpec,
    dns_required: bool,
    credit: &HashMap<NodeId, Resources>,
) -> usize {
    let resources = scheduler_resources(spec);
    let required = spec
        .placement
        .as_ref()
        .map(|p| crate::meat::scheduler::parse_label_list(&p.required))
        .unwrap_or_default();
    let requires_egress = spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty());
    crate::meat::scheduler::daemon_candidates(
        cache,
        app_id,
        credit,
        &resources,
        &required,
        requires_egress,
        dns_required,
    )
    .len()
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
/// This task reads async Raft state and metrics, then drives the pure
/// `AutoscaleConfig::from_spec`, `evaluate` and `AutoscaleTracker` functions.
/// Tracker changes follow confirmed Raft writes, so refusal can be retried.
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
                let config = match AutoscaleConfig::from_spec(autoscale, spec.cpu, spec.memory) {
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

                // Utilisation of the app's request over the CONFIGURED window
                // (was hardcoded to five minutes regardless of the spec).
                let Some(metric) = app_metric_utilisation(&rollup_store, &config, app_id).await
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

/// Average utilisation of the app's resource request over the app's
/// `[autoscale] evaluation_window`, as a fraction, from the leader's
/// rollup store.
///
/// Reads the per-instance series the node collector really records
/// (`process_cpu_percent`, percent of one core, or `process_memory_bytes`),
/// averages it across the app's instances and minutes, then divides by the
/// per-replica request: 1.0 means each replica uses exactly what it asked
/// for. The autoscaler used to query a series literally named `cpu`, which
/// nothing records, so it never scaled a real cluster.
///
/// Returns `None` when there's no data.
async fn app_metric_utilisation(
    rollup_store: &tokio::sync::RwLock<crate::mayo::rollup_store::RollupStore>,
    config: &crate::meat::autoscaler::AutoscaleConfig,
    app_id: &crate::meat::types::AppId,
) -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let window_start = now.saturating_sub(config.evaluation_window.as_secs());

    let store = rollup_store.read().await;
    let aggregates = store
        .query_cluster_aggregates(config.metric.series_name(), window_start, now)
        .await
        .ok()?;
    let mut total = 0.0;
    let mut n = 0u64;
    for agg in &aggregates {
        if agg.count > 0 && aggregate_is_for_app(&agg.labels, app_id) {
            total += agg.sum / f64::from(agg.count);
            n += 1;
        }
    }
    if n == 0 {
        None
    } else {
        Some(config.utilisation(total / n as f64))
    }
}

/// Whether a rollup aggregate's labels belong to exactly `app_id` (M26).
///
/// The collector labels per-app metrics with `app = "<namespace>/<app>"`
/// (`mayo::collector`). The old autoscaler used `labels.contains(bare_name)`,
/// a substring match that pooled `web` with `webhook`/`web-api` and merged the
/// same app name across namespaces. Parse the labels JSON and compare the
/// `app` field to the namespace-qualified id exactly.
fn aggregate_is_for_app(labels_json: &str, app_id: &crate::meat::types::AppId) -> bool {
    let qualified = format!("{}/{}", app_id.namespace, app_id.name);
    serde_json::from_str::<serde_json::Value>(labels_json)
        .ok()
        .and_then(|v| v.get("app").and_then(|a| a.as_str()).map(String::from))
        .is_some_and(|app_label| app_label == qualified)
}

/// Build the cluster-wide service endpoint catalogue from node reports.
///
/// Every node reports its `running_apps` (namespace, app, host port,
/// health) through the reporting tree; the leader already reads the
/// aggregated result. This walks those reports, groups the instances by
/// namespaced [`ServiceId`], and records each as a backend — the real node
/// IP from gossip membership, the host port and the health flag. The
/// declared container port comes from the desired-state `AppSpec` (a
/// report only carries the host port). VIPs are then allocated
/// cluster-wide by the catalogue, preserving existing allocations before
/// adding newcomers. Declared services retain their VIP even when reports
/// temporarily contain no running backend, and a live node that hasn't
/// reported under this leader yet keeps its committed backends.
///
/// Only services whose app declares a port appear: a portless app has no
/// VIP and nothing to resolve.
fn build_endpoint_catalog(
    members: &[MembershipSnapshot],
    reports: &AggregatedState,
    desired: &crate::council::types::DesiredState,
) -> Result<crate::onion::catalog::EndpointCatalog, crate::onion::types::OnionError> {
    use crate::onion::catalog::CatalogBackend;
    use crate::onion::service_id::ServiceId;
    use crate::reporting::types::ReportHealthStatus;

    // node_id -> its IPv4 address, from gossip membership.
    let node_ips: std::collections::HashMap<&NodeId, std::net::Ipv4Addr> = members
        .iter()
        .filter_map(|m| match m.address.ip() {
            std::net::IpAddr::V4(v4) => Some((&m.node_id, v4)),
            std::net::IpAddr::V6(_) => None,
        })
        .collect();

    // Qualified id -> (ServiceId, declared port, backends). A BTreeMap keyed
    // by the qualified string keeps the build deterministic.
    let mut grouped: BTreeMap<String, (ServiceId, u16, Vec<CatalogBackend>)> = desired
        .apps
        .iter()
        .filter_map(|(app, spec)| {
            spec.port.map(|port| {
                let id = ServiceId::new(&app.namespace, &app.name);
                (id.qualified(), (id, port, Vec::new()))
            })
        })
        .collect();
    for (node_id, report) in &reports.reports {
        if reports.stale_nodes.contains(node_id) {
            continue;
        }
        let Some(&node_ip) = node_ips.get(node_id) else {
            continue; // no known IP (departed, or IPv6-only) — can't route to it
        };
        for app in &report.running_apps {
            if desired
                .producer_retirements
                .blocks(&node_id.0, app.execution.as_ref())
            {
                continue;
            }
            let Some(host_port) = app.port else {
                continue; // portless instance: nothing to resolve
            };
            let service_id = ServiceId::new(&app.namespace, &app.app_name);
            // The declared container port lives in the desired spec.
            let app_id = crate::meat::types::AppId::new(&app.app_name, &app.namespace);
            let Some(declared_port) = desired.apps.get(&app_id).and_then(|s| s.port) else {
                continue;
            };
            let healthy = matches!(app.health_status, ReportHealthStatus::Healthy);
            grouped
                .entry(service_id.qualified())
                .or_insert((service_id, declared_port, Vec::new()))
                .2
                .push(CatalogBackend {
                    execution: app.execution.clone(),
                    node_id: node_id.0.clone(),
                    node_ip,
                    host_port,
                    healthy,
                });
        }
    }

    // A member gossip still counts, but that hasn't reported under this
    // leader, keeps the backends the committed catalogue gave it. A fresh
    // leader starts with no reports at all and a restarted agent takes a few
    // seconds to send its first, while the containers behind those backends
    // carry on serving. Dropping them would make every consumer's connect
    // hook refuse live services until the reports arrived. The node's own
    // report stays authoritative the moment it lands, and a producer
    // retirement still withdraws a backend here.
    for (qualified, service) in &desired.endpoint_catalog.services {
        let Some((_, _, backends)) = grouped.get_mut(qualified) else {
            continue; // no longer a declared service
        };
        for backend in &service.backends {
            let node_id = NodeId::new(&backend.node_id);
            let still_there = members.iter().any(|member| {
                member.node_id == node_id
                    && matches!(member.state, NodeState::Alive | NodeState::Suspect)
                    && member.address.ip() == std::net::IpAddr::V4(backend.node_ip)
            });
            if still_there
                && !reports.stale_nodes.contains(&node_id)
                && !reports.reports.contains_key(&node_id)
                && !desired
                    .producer_retirements
                    .blocks(&backend.node_id, backend.execution.as_ref())
            {
                backends.push(backend.clone());
            }
        }
    }

    // A departing service may still be present on a remote node. Reserve both
    // existing allocations and already-recorded withdrawals before probing.
    let reserved = desired.endpoint_withdrawals.reserved_vips().chain(
        desired
            .endpoint_catalog
            .services
            .values()
            .map(|service| service.vip),
    );
    desired
        .endpoint_catalog
        .reconcile_reserving(grouped.into_values(), reserved)
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
        let capability_report = reports.capabilities.get(&member.node_id);
        if usage.cpu_total_millicores == 0 {
            continue; // capacity unset
        }

        // One entry per running instance, so this counts replicas.
        let mut app_replicas = HashMap::new();
        for app in &report.running_apps {
            *app_replicas
                .entry(crate::meat::types::AppId::new(
                    &app.app_name,
                    &app.namespace,
                ))
                .or_default() += 1;
        }

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
            ready: reports
                .readiness
                .get(&member.node_id)
                .is_some_and(|report| report.evidence.ready)
                && capability_report.is_none_or(|capability| !capability.egress_degraded),
            capabilities: capability_report
                .map(|capability| capability.capabilities)
                .unwrap_or_default(),
            app_replicas,
            uptime_secs: member.first_seen.elapsed().as_secs(),
            // Nothing reports cached images yet; locality scoring is
            // inert rather than fed guesses.
            cached_images: HashSet::new(),
        });
    }
    cache
}

/// Ask the agent whether instances it adopted at startup already run `spec`.
/// No answer in time counts as "no", which deploys as before.
async fn adopted_instances_match(
    cmd_tx: &mpsc::Sender<AgentCommand>,
    key: &(String, String),
    spec: &crate::config::app::AppSpec,
    io_timeout: Duration,
) -> bool {
    let ask = async {
        let (response, answer) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(AgentCommand::AdoptedPlacementMatches {
                app_name: key.0.clone(),
                namespace: key.1.clone(),
                spec: Box::new(spec.clone()),
                response,
            })
            .await
            .ok()?;
        answer.await.ok()
    };
    matches!(tokio::time::timeout(io_timeout, ask).await, Ok(Some(true)))
}

/// Recheck convergence after restart without forgetting owned resources.
/// Missing or incomplete runtime inventory returns an assignment to Pending.
pub fn retain_live_assignments(
    applied: &mut AppliedMap,
    statuses: &[crate::bun::agent::InstanceStatus],
) {
    for ((name, namespace), state) in applied {
        let AssignmentState::Applied { fingerprint } = state else {
            continue;
        };
        let expected = serde_json::from_str::<AppSpec>(fingerprint)
            .ok()
            .and_then(|spec| {
                if let Replicas::Fixed(count) = spec.replicas {
                    Some(count)
                } else {
                    None
                }
            });
        let active = statuses
            .iter()
            .filter(|instance| {
                &instance.app_name == name
                    && &instance.namespace == namespace
                    && matches!(
                        instance.state.as_str(),
                        "pending"
                            | "preparing"
                            | "initialising"
                            | "starting"
                            | "health-wait"
                            | "running"
                            | "unhealthy"
                    )
            })
            .count();
        if expected.is_none_or(|expected| active != expected as usize) {
            *state = AssignmentState::Pending;
        }
    }
}

async fn persist_placements(
    path: Option<&std::path::Path>,
    owned: &AppliedMap,
) -> std::io::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let path = path.to_path_buf();
    let owned = owned.clone();
    tokio::task::spawn_blocking(move || crate::cluster::applied::save(&path, &owned))
        .await
        .map_err(std::io::Error::other)?
}

/// One confirmed placement poll: the leader's answer, once its catalogue is
/// published locally.
struct ConsumerPoll {
    leader_url: String,
    assignments: NodeAssignments,
    /// The agent registers services only at committed allocations, so a
    /// placement must wait for its allocation to reach the catalogue.
    requires_allocations: bool,
}

// Discovery must keep progressing while a rollout waits for its terminal event.
#[allow(clippy::too_many_arguments)]
async fn poll_consumer(
    node_name: &str,
    metrics_rx: &watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    directory_rx: &watch::Receiver<crate::mustard::directory::NodeDirectory>,
    raft_to_api_offset: i32,
    service_token: &Option<String>,
    cmd_tx: &mpsc::Sender<AgentCommand>,
    shutdown: &CancellationToken,
    cluster_http: &crate::cluster::ClusterHttp,
    receipt_cursor: &mut usize,
    io_timeout: Duration,
) -> Option<ConsumerPoll> {
    let client = cluster_http.client();
    let leader_url = {
        let metrics = metrics_rx.borrow();
        let directory = directory_rx.borrow();
        crate::cluster::directory::resolve_leader(&metrics, &directory, raft_to_api_offset, 0)
            .and_then(|view| view.api_address)
            .map(|address| cluster_http.url(&address.to_string(), ""))
    };
    let leader_url = leader_url?;

    let url = format!("{leader_url}/v1/placements/{node_name}");
    let mut request = client.get(&url);
    if let Some(token) = service_token {
        request = request.bearer_auth(token);
    }
    // The view lease runs from before the request leaves, so it can only
    // end earlier than the leader's own count of this node's silence.
    let requested_at_ns = crate::onion::lease::boot_clock_ns();
    // The deadline covers both headers and body. An incomplete body
    // must not prevent the next placement poll or graceful shutdown.
    let poll = async {
        request
            .send()
            .await?
            .error_for_status()?
            .json::<NodeAssignments>()
            .await
    };
    let polled = tokio::select! {
        _ = shutdown.cancelled() => return None,
        result = tokio::time::timeout(io_timeout, poll) => result,
    };
    let assignments = match polled {
        Ok(Ok(assignments)) => assignments,
        _ => return None,
    };

    // Queue acceptance is not publication. The deadline covers both
    // sending the update and receiving the agent's confirmed result.
    let sync_catalogue = async {
        let (response, reply) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(AgentCommand::SyncClusterConsumer {
                generation: assignments.endpoint_generation,
                catalog: Box::new(assignments.endpoint_catalog.clone()),
                ingress: assignments.ingress.clone(),
                withdrawals: assignments.endpoint_withdrawals.clone(),
                requested_at_ns,
                response,
            })
            .await
            .map_err(|_| crate::bun::BunError::ClusterPublication("agent channel closed".into()))?;
        reply
            .await
            .map_err(|_| crate::bun::BunError::ClusterPublication("agent reply lost".into()))?
    };
    let synchronised = tokio::select! {
        _ = shutdown.cancelled() => return None,
        result = tokio::time::timeout(io_timeout, sync_catalogue) => result,
    };
    let update = match synchronised {
        Ok(Ok(update)) => update,
        Ok(Err(error)) => {
            eprintln!("orchestrator: {error}");
            return None;
        }
        Err(_) => {
            eprintln!("orchestrator: cluster discovery publication timed out");
            return None;
        }
    };

    // Rotate bounded batches so a failing receipt cannot starve later generations.
    let receipt_http = cluster_http.clone().with_bearer(service_token.clone());
    let count = update.receipts.len();
    if count > 0 {
        for offset in 0..count.min(16) {
            let generation = update.receipts[(*receipt_cursor + offset) % count];
            let deliver = async {
                super::consumer::acknowledge(&receipt_http, &leader_url, generation)
                    .await
                    .ok()?;
                let (response, reply) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(AgentCommand::ConfirmConsumerReceipt {
                        generation,
                        response,
                    })
                    .await
                    .ok()?;
                reply.await.ok()?.ok()
            };
            tokio::select! {
                _ = shutdown.cancelled() => return None,
                _ = tokio::time::timeout(Duration::from_secs(1), deliver) => {}
            }
        }
        *receipt_cursor = (*receipt_cursor + count.min(16)) % count;
    }
    if !update.published {
        return None;
    }
    Some(ConsumerPoll {
        leader_url,
        assignments,
        requires_allocations: update.requires_allocations,
    })
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
    // Production nodes persist ownership before runtime mutation. `None` is
    // for ephemeral embedded tests and cannot provide restart recovery.
    state_dir: Option<std::path::PathBuf>,
    // The agent's `[runtime] stop_confirmation_timeout_secs`, which bounds
    // how long a retirement may take.
    stop_confirmation_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    spawn_placement_reconciler_with_io_timeout(
        node_name,
        metrics_rx,
        directory_rx,
        raft_to_api_offset,
        service_token,
        cmd_tx,
        shutdown,
        cluster_http,
        state_dir,
        RECONCILE_IO_TIMEOUT,
        retire_timeout(RECONCILE_IO_TIMEOUT, stop_confirmation_timeout),
    )
}

/// [`spawn_placement_reconciler`] with an explicit deadline for each leader
/// request and agent reply, and for each retirement, so tests of a stalled
/// peer need not wait out the production deadlines.
#[allow(clippy::too_many_arguments)]
fn spawn_placement_reconciler_with_io_timeout(
    node_name: String,
    metrics_rx: watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>,
    directory_rx: watch::Receiver<crate::mustard::directory::NodeDirectory>,
    raft_to_api_offset: i32,
    service_token: Option<String>,
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
    cluster_http: crate::cluster::ClusterHttp,
    state_dir: Option<std::path::PathBuf>,
    io_timeout: Duration,
    retire_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = cluster_http.client().clone();
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        let checkpoint_path = state_dir
            .as_deref()
            .map(crate::cluster::applied::checkpoint_path);
        let mut applied = loop {
            let loaded = if let Some(path) = checkpoint_path.clone() {
                tokio::task::spawn_blocking(move || crate::cluster::applied::load(&path))
                    .await
                    .map_err(std::io::Error::other)
                    .and_then(|result| result)
            } else {
                Ok(AppliedMap::new())
            };
            match loaded {
                Ok(owned) => break owned,
                Err(error) => eprintln!("orchestrator: cannot load placement ownership: {error}"),
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
            }
        };
        let mut checkpoint_verified = false;
        let mut receipt_cursor = 0usize;
        // A deploy that fails (an image whose process exits at once, say)
        // waits before the same specification is tried again, instead of
        // being redeployed on every poll.
        let mut backoff = super::deploy_backoff::DeployBackoff::default();
        // Placements already reported as waiting for their allocation, so
        // the wait is logged once rather than every poll.
        let mut awaiting: HashSet<(String, String)> = HashSet::new();
        let retirer = Retirer {
            node_name: &node_name,
            cmd_tx: &cmd_tx,
            client: &client,
            service_token: &service_token,
            checkpoint_path: checkpoint_path.as_deref(),
            shutdown: &shutdown,
            io_timeout,
            retire_timeout,
        };

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
            if !checkpoint_verified {
                let inventory = async {
                    let (response, received) = tokio::sync::oneshot::channel();
                    cmd_tx.send(AgentCommand::Status { response }).await.ok()?;
                    received.await.ok()
                };
                let Ok(Some(statuses)) =
                    tokio::time::timeout(Duration::from_secs(5), inventory).await
                else {
                    continue;
                };
                retain_live_assignments(&mut applied, &statuses);
                if let Err(error) = persist_placements(checkpoint_path.as_deref(), &applied).await {
                    eprintln!("orchestrator: cannot persist recovered ownership: {error}");
                    continue;
                }
                checkpoint_verified = true;
            }

            let Some(ConsumerPoll {
                leader_url,
                assignments,
                requires_allocations,
            }) = poll_consumer(
                &node_name,
                &metrics_rx,
                &directory_rx,
                raft_to_api_offset,
                &service_token,
                &cmd_tx,
                &shutdown,
                &cluster_http,
                &mut receipt_cursor,
                io_timeout,
            )
            .await
            else {
                continue;
            };

            // Placements already gone from this node retire before anything
            // deploys: that costs no availability, and a deploy may take
            // minutes (a lease's cleanup bound can't include them).
            if retirer
                .retire_departed(&mut applied, &leader_url, &assignments, None)
                .await
                .is_none()
            {
                return;
            }

            let seen = assigned_keys(&assignments);
            // Placements a poll taken during this cycle no longer assigns
            // here. Deploying them from this cycle's older answer could start
            // a volume app on a node the leader has already moved it from.
            let mut withdrawn: HashSet<(String, String)> = HashSet::new();
            for assignment in &assignments.apps {
                let key = (assignment.name.clone(), assignment.namespace.clone());
                if withdrawn.contains(&key) {
                    continue;
                }

                let mut spec = assignment.spec.clone();
                // The local agent runs exactly this node's share, under the
                // ordinals the leader gave it.
                spec.replicas = Replicas::Fixed(assignment.ordinals.len() as u32);
                spec.ordinals = Some(assignment.ordinals.clone());
                let fingerprint = serde_json::to_string(&spec).unwrap_or_default();
                if matches!(applied.get(&key), Some(AssignmentState::Applied { fingerprint: previous }) if previous == &fingerprint)
                {
                    continue; // already converged; don't redeploy
                }
                // The leader commits a placement and its service allocation in
                // separate writes, so an answer can place an app whose
                // allocation hasn't committed yet (#309). The agent would refuse
                // to register the service; a later poll carries it.
                if requires_allocations
                    && awaits_allocation(&assignments.endpoint_catalog, assignment)
                {
                    if awaiting.insert(key.clone()) {
                        eprintln!(
                            "orchestrator: {}/{} is placed here but its service allocation \
                             hasn't committed yet; deploying once it has",
                            key.0, key.1
                        );
                    }
                    continue;
                }
                awaiting.remove(&key);

                if !backoff.may_attempt(&key, &fingerprint, std::time::Instant::now()) {
                    continue;
                }
                // A restart or self-upgrade between queueing a deploy and
                // recording it applied leaves the entry pending while the
                // adopted instances already run it. Rolling them again would
                // replace every replica for nothing (and surge a second writer
                // onto a volume app's data before #267).
                if adopted_instances_match(&cmd_tx, &key, &spec, io_timeout).await {
                    let mut next = applied.clone();
                    next.insert(key.clone(), AssignmentState::Applied { fingerprint });
                    match persist_placements(checkpoint_path.as_deref(), &next).await {
                        Ok(()) => {
                            applied = next;
                            backoff.clear(&key);
                            eprintln!(
                                "orchestrator: adopted instances of {}/{} already run their placement; not redeploying",
                                key.0, key.1
                            );
                        }
                        Err(error) => eprintln!("orchestrator: cannot record convergence: {error}"),
                    }
                    continue;
                }

                let mut config = Config::default();
                config.app.insert(assignment.name.clone(), spec);

                // The durable intent survives a crash after the agent accepts
                // work but before we can observe its terminal outcome.
                let mut next = applied.clone();
                next.insert(key.clone(), AssignmentState::Pending);
                if let Err(error) = persist_placements(checkpoint_path.as_deref(), &next).await {
                    eprintln!("orchestrator: cannot record placement ownership: {error}");
                    continue;
                }
                applied = next;
                let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32);
                let deploy = cmd_tx.send(AgentCommand::Deploy {
                    config,
                    events: event_tx,
                });
                let queued = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    result = tokio::time::timeout(io_timeout, deploy) => result,
                };
                if !matches!(queued, Ok(Ok(()))) {
                    continue;
                }
                // The deploy's events drain on their own task, so retiring
                // below can't stall the stream: the agent closes a stream its
                // reader leaves full, and the deploy would then look failed.
                let mut terminal = tokio::spawn(deploy_outcome(event_rx, DEPLOY_TERMINAL_TIMEOUT));
                let outcome = loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => {
                            terminal.abort();
                            return;
                        }
                        result = &mut terminal => {
                            break result.unwrap_or_else(|error| {
                                Err(DeployWaitError::WatcherLost(error.to_string()))
                            });
                        }
                        _ = tick.tick() => {
                            // The producer can need our own withdrawal receipt before
                            // it can emit the terminal deployment event.
                            let Some(ConsumerPoll { leader_url: fresh_url, assignments: fresh, .. }) = poll_consumer(
                                &node_name, &metrics_rx, &directory_rx, raft_to_api_offset,
                                &service_token, &cmd_tx, &shutdown, &cluster_http,
                                &mut receipt_cursor, io_timeout,
                            ).await else {
                                continue;
                            };
                            withdrawn.extend(seen.difference(&assigned_keys(&fresh)).cloned());
                            // A retirement must not wait out a deploy of some other app.
                            let retired = retirer
                                .retire_departed(&mut applied, &fresh_url, &fresh, Some(&key))
                                .await;
                            if retired.is_none() {
                                terminal.abort();
                                return;
                            }
                        }
                    }
                };
                if let Err(error) = outcome {
                    let deferral =
                        backoff.record_failure(&key, &fingerprint, std::time::Instant::now());
                    eprintln!(
                        "orchestrator: deploy of {}/{} failed (attempt {}), retrying in {}s: {error}",
                        key.0,
                        key.1,
                        deferral.failures,
                        deferral.delay.as_secs()
                    );
                    continue;
                }
                backoff.clear(&key);
                let mut next = applied.clone();
                next.insert(key, AssignmentState::Applied { fingerprint });
                match persist_placements(checkpoint_path.as_deref(), &next).await {
                    Ok(()) => applied = next,
                    Err(error) => eprintln!("orchestrator: cannot record convergence: {error}"),
                }
            }

            backoff.retain(|key| seen.contains(key));
            awaiting.retain(|key| seen.contains(key));
        }
    })
}

/// Whether `assignment` declares a port that `catalog` doesn't allocate yet,
/// at that port. The agent registers a clustered service only at its
/// committed allocation, so deploying before it arrives can only fail.
fn awaits_allocation(
    catalog: &crate::onion::catalog::EndpointCatalog,
    assignment: &NodeAssignment,
) -> bool {
    let Some(port) = assignment.spec.port else {
        return false;
    };
    let service = crate::onion::service_id::ServiceId::new(&assignment.namespace, &assignment.name);
    catalog
        .resolve(&service)
        .is_none_or(|allocation| allocation.port != port)
}

/// The (name, namespace) of every app `assignments` places on this node.
fn assigned_keys(assignments: &NodeAssignments) -> HashSet<(String, String)> {
    assignments
        .apps
        .iter()
        .map(|assignment| (assignment.name.clone(), assignment.namespace.clone()))
        .collect()
}

/// What one node's placement reconciler needs to retire the placements that
/// have left the node. It only borrows the reconciler's own values.
struct Retirer<'a> {
    node_name: &'a str,
    cmd_tx: &'a mpsc::Sender<AgentCommand>,
    client: &'a reqwest::Client,
    service_token: &'a Option<String>,
    checkpoint_path: Option<&'a std::path::Path>,
    shutdown: &'a CancellationToken,
    io_timeout: Duration,
    retire_timeout: Duration,
}

impl Retirer<'_> {
    /// Retire every placement this node owns that `assignments` no longer
    /// lists, and every lease retirement the leader asks of this node, then
    /// acknowledge each confirmed lease retirement to the leader.
    ///
    /// `busy` names a placement whose deploy is still in flight. It waits
    /// for a later cycle, so no app ever has a deploy and a retirement
    /// outstanding at once. Returns `None` if shutdown interrupted it.
    async fn retire_departed(
        &self,
        applied: &mut AppliedMap,
        leader_url: &str,
        assignments: &NodeAssignments,
        busy: Option<&(String, String)>,
    ) -> Option<()> {
        let seen = assigned_keys(assignments);
        // The leader retains owners across rescheduling and local journal
        // loss. Its instructions therefore supplement our local inventory.
        let mut removed: BTreeMap<_, Vec<LeaseRetirement>> = applied
            .keys()
            .filter(|key| !seen.contains(*key) && Some(*key) != busy)
            .map(|key| (key.clone(), Vec::new()))
            .collect();
        for retirement in &assignments.retirements {
            let app = &retirement.placement.app_id;
            let key = (app.name.clone(), app.namespace.clone());
            if retirement.placement.node_id.0 != self.node_name || seen.contains(&key) {
                eprintln!("orchestrator: refusing conflicting retirement instruction");
                continue;
            }
            if Some(&key) != busy {
                removed.entry(key).or_default().push(retirement.clone());
            }
        }
        let retire_timeout = self.retire_timeout;
        // Retirements run side by side: each may wait out a stubborn
        // workload's stop grace, and one must not hold up the rest.
        // Each future owns its inputs: a spawned task can't hold futures
        // that borrow from a closure's arguments.
        let mut retirements = futures_util::stream::iter(removed.into_iter().map(
            |((name, namespace), confirmations)| {
                let cmd_tx = self.cmd_tx.clone();
                async move {
                    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                    // Queueing and acknowledgement share one deadline. An unknown
                    // outcome keeps ownership and lets other owners progress.
                    let retire = async {
                        let command = if confirmations.is_empty() {
                            AgentCommand::Retire {
                                app_name: name.clone(),
                                namespace: namespace.clone(),
                                response: response_tx,
                            }
                        } else {
                            AgentCommand::RetireTestResources {
                                app_name: name.clone(),
                                namespace: namespace.clone(),
                                response: response_tx,
                            }
                        };
                        cmd_tx
                            .send(command)
                            .await
                            .map_err(|_| "agent command channel closed")?;
                        response_rx
                            .await
                            .map_err(|_| "agent dropped retirement response")
                    };
                    let retired = tokio::time::timeout(retire_timeout, retire).await;
                    (name, namespace, confirmations, retired)
                }
            },
        ))
        .buffer_unordered(MAX_CONCURRENT_RETIREMENTS);
        loop {
            let next = tokio::select! {
                _ = self.shutdown.cancelled() => return None,
                next = retirements.next() => next,
            };
            let Some((name, namespace, confirmations, retired)) = next else {
                return Some(());
            };
            let key = (name, namespace);
            match retired {
                Ok(Ok(Ok(()))) => {
                    let mut next = applied.clone();
                    next.remove(&key);
                    if let Err(error) = persist_placements(self.checkpoint_path, &next).await {
                        eprintln!("orchestrator: cannot record retirement: {error}");
                        continue;
                    }
                    *applied = next;
                    for confirmation in confirmations {
                        self.acknowledge(leader_url, &confirmation).await?;
                    }
                }
                Ok(Ok(Err(e))) => {
                    eprintln!(
                        "orchestrator: retirement of {}/{} failed, will retry: {e}",
                        key.0, key.1
                    );
                    forget_convergence(applied, key, self.checkpoint_path).await;
                }
                Ok(Err(error)) => {
                    eprintln!(
                        "orchestrator: retirement of {}/{}: {error}; will retry",
                        key.0, key.1
                    );
                    forget_convergence(applied, key, self.checkpoint_path).await;
                }
                Err(_) => {
                    eprintln!(
                        "orchestrator: retirement of {}/{} exceeded {retire_timeout:?}; ownership retained",
                        key.0, key.1
                    );
                    forget_convergence(applied, key, self.checkpoint_path).await;
                }
            }
        }
    }

    /// Tell the leader this node has confirmed one lease retirement. A lost
    /// acknowledgement only means the leader asks again. Returns `None` if
    /// shutdown interrupted it.
    async fn acknowledge(&self, leader_url: &str, confirmation: &LeaseRetirement) -> Option<()> {
        let mut request = self
            .client
            .post(format!("{leader_url}/v1/test/leases/retired"))
            .json(confirmation);
        if let Some(token) = self.service_token {
            request = request.bearer_auth(token);
        }
        let acknowledged = tokio::select! {
            _ = self.shutdown.cancelled() => return None,
            result = tokio::time::timeout(self.io_timeout, request.send()) => result,
        };
        if !matches!(acknowledged, Ok(Ok(ref response)) if response.status() == reqwest::StatusCode::NO_CONTENT)
        {
            eprintln!(
                "orchestrator: lease retirement acknowledgement failed; leader retains ownership"
            );
        }
        Some(())
    }
}

/// Keep ownership of a workload whose retirement did not finish, but stop
/// calling it converged.
///
/// A retirement that fails part-way has usually stopped the replicas already
/// (the address release is what waits on other nodes). If the leader then
/// hands the same assignment back, as it does when a node it briefly gave up
/// on reports again, an `Applied` fingerprint would match and the stopped
/// replicas would never be started. `Pending` still retires the workload if
/// the assignment stays gone, and deploys it if the assignment returns.
async fn forget_convergence(
    applied: &mut AppliedMap,
    key: (String, String),
    checkpoint_path: Option<&std::path::Path>,
) {
    let Some(state) = applied.get_mut(&key) else {
        return;
    };
    if matches!(state, AssignmentState::Pending) {
        return;
    }
    *state = AssignmentState::Pending;
    // The in-memory state already drives this process; a failed write only
    // means a restart re-derives convergence from the runtime inventory.
    if let Err(error) = persist_placements(checkpoint_path, applied).await {
        eprintln!("orchestrator: cannot record unfinished retirement: {error}");
    }
}

/// How long the reconciler waits for a deploy's terminal event before giving
/// up on this tick (M14). Without a bound, a deploy that stalls (a stuck image
/// pull, a hung runtime holding the event sender) would block the reconcile
/// tick forever: the node would stop polling the leader, never converge other
/// apps, and silently drop out of reconciliation while still alive. On timeout
/// the placement is treated as not-yet-applied and retried next tick.
const DEPLOY_TERMINAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a deploy the reconciler handed to the agent did not converge.
#[derive(Debug, thiserror::Error)]
enum DeployWaitError {
    /// The agent reported the deploy failed, with its reason.
    #[error("{0}")]
    Failed(String),
    /// The agent dropped the event stream without a terminal event.
    #[error("the agent closed the deploy's event stream without an outcome")]
    Closed,
    /// No terminal event arrived in time.
    #[error("the deploy did not reach a terminal event within {}s", .0.as_secs())]
    TimedOut(Duration),
    /// The task reading the event stream ended without an outcome.
    #[error("the deploy's event reader ended without an outcome: {0}")]
    WatcherLost(String),
}

/// Drain a deploy's event stream until it reaches `Complete` within `timeout`.
///
/// Every error leaves the placement unapplied, so the caller retries it next
/// tick. `Failed` carries the agent's own message, so the caller can say why.
async fn deploy_outcome(
    mut events: mpsc::Receiver<ApplyEvent>,
    timeout: Duration,
) -> Result<(), DeployWaitError> {
    let drain = async {
        while let Some(event) = events.recv().await {
            match event {
                ApplyEvent::Complete { .. } => return Ok(()),
                ApplyEvent::Error { message } => return Err(DeployWaitError::Failed(message)),
                _ => {}
            }
        }
        Err(DeployWaitError::Closed)
    };
    tokio::time::timeout(timeout, drain)
        .await
        .unwrap_or(Err(DeployWaitError::TimedOut(timeout)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::types::{ResourceUsage, StateReport};
    use std::collections::HashMap;
    use std::time::{Instant, SystemTime};

    fn withdrawals_owed_by(
        consumers: &[&str],
        generations: u64,
    ) -> crate::onion::withdrawal::EndpointWithdrawals {
        use crate::onion::withdrawal::{EndpointWithdrawal, EndpointWithdrawals};
        EndpointWithdrawals {
            generation: generations + 1,
            pending: (1..=generations)
                .map(|generation| {
                    (
                        generation,
                        EndpointWithdrawal {
                            services: Default::default(),
                            consumers: consumers.iter().map(|c| c.to_string()).collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn withdrawal_ledger_gauge_exports_the_leader_reading() {
        let gauge = WithdrawalLedgerGauge::default();
        gauge.record(&withdrawals_owed_by(&["lost"], 512));
        assert_eq!(
            gauge.samples(),
            [
                ("discovery_withdrawal_ledger_occupancy_ratio", 0.5),
                ("discovery_withdrawal_pending_generations", 512.0),
            ]
        );
        gauge.clear();
        assert_eq!(gauge.samples()[0].1, 0.0);
    }

    #[test]
    fn withdrawal_backlog_is_quiet_below_three_quarters() {
        let withdrawals = withdrawals_owed_by(&["lost"], 700);
        assert!(withdrawal_backlog_warning(&withdrawals, &HashSet::new()).is_none());
    }

    #[test]
    fn withdrawal_backlog_names_the_node_that_owes_receipts() {
        let mut withdrawals = withdrawals_owed_by(&["lost", "worker"], 800);
        for withdrawal in withdrawals.pending.values_mut().skip(10) {
            withdrawal.consumers.remove("worker");
        }
        let warning = withdrawal_backlog_warning(&withdrawals, &HashSet::from(["worker"])).unwrap();
        assert!(warning.contains("78% full"), "{warning}");
        assert!(
            warning.contains("lost (800 generations, not alive), worker (10 generations, alive)"),
            "{warning}"
        );
        assert!(warning.contains("relish decommission-node"), "{warning}");
    }

    fn reconciler_for_deadline_test(
        address: std::net::SocketAddr,
        directory: &std::path::Path,
        commands: mpsc::Sender<AgentCommand>,
    ) -> tokio::task::JoinHandle<()> {
        reconciler_with_retire_deadline(address, directory, commands, Duration::from_secs(2))
    }

    fn reconciler_with_retire_deadline(
        address: std::net::SocketAddr,
        directory: &std::path::Path,
        commands: mpsc::Sender<AgentCommand>,
        retire_timeout: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let (_, metrics_rx) = watch::channel(openraft::RaftMetrics::new_initial(1));
        let (_, directory_rx) = watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: NodeId::new("leader"),
                term: 1,
                recovery_epoch: 0,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        // Every stall these tests inject is permanent, so a shorter deadline
        // proves the same bound without waiting out the production ten
        // seconds. Two seconds still leaves a loaded runner's local round
        // trips well inside it; a spurious expiry only retries next tick.
        spawn_placement_reconciler_with_io_timeout(
            "worker".into(),
            metrics_rx,
            directory_rx,
            0,
            None,
            commands,
            CancellationToken::new(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(directory.to_path_buf()),
            Duration::from_secs(2),
            retire_timeout,
        )
    }

    /// The production retirement deadline outlasts a stop that waits out the
    /// whole grace and then force-kills, plus time queued behind other work.
    #[test]
    fn retirement_deadline_outlasts_a_stubborn_stop() {
        let confirmation =
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout();
        let deadline = retire_timeout(RECONCILE_IO_TIMEOUT, confirmation);
        assert!(deadline > crate::bun::agent::stop_completion_bound(confirmation));
        assert!(deadline > RECONCILE_IO_TIMEOUT);
    }

    /// A released lease's owner first waits for its next poll, then retires,
    /// then acknowledges: the bound covers all three, not the retirement alone.
    #[test]
    fn lease_retirement_bound_covers_poll_retirement_and_acknowledgement() {
        let confirmation =
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout();
        let bound = lease_retirement_bound(confirmation);
        assert!(bound > retire_timeout(RECONCILE_IO_TIMEOUT, confirmation) + RECONCILE_INTERVAL);
        assert!(bound > Duration::from_secs(30), "{bound:?}");
    }

    /// V02 soak: retirements of SIGTERM-ignoring apps ran one at a time, each
    /// timing out ("exceeded ten seconds") before its stop could finish. A
    /// cycle's retirements now wait side by side, so three stops that each
    /// take one grace finish in about one grace, all in the first cycle.
    #[tokio::test]
    async fn a_cycles_retirements_wait_out_their_stops_side_by_side() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(|| async { axum::Json(NodeAssignments::default()) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        let apps = ["first", "second", "third"];
        crate::cluster::applied::save(
            &checkpoint,
            &apps
                .iter()
                .map(|app| {
                    (
                        (app.to_string(), "default".to_string()),
                        AssignmentState::Pending,
                    )
                })
                .collect(),
        )
        .unwrap();
        let grace = Duration::from_millis(1500);
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_with_retire_deadline(address, root.path(), commands, grace * 2);
        // A stand-in agent whose every stop waits out the grace, concurrently.
        let retirements = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = retirements.clone();
        let agent = tokio::spawn(async move {
            while let Some(command) = received.recv().await {
                match command {
                    AgentCommand::Status { response } => {
                        let _ = response.send(vec![]);
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Retire {
                        app_name, response, ..
                    } => {
                        recorded
                            .lock()
                            .unwrap()
                            .push((app_name, std::time::Instant::now()));
                        tokio::spawn(async move {
                            tokio::time::sleep(grace).await;
                            let _ = response.send(Ok(()));
                        });
                    }
                    _ => {}
                }
            }
        });

        let retired = tokio::time::timeout(Duration::from_secs(15), async {
            while !crate::cluster::applied::load(&checkpoint)
                .unwrap()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            std::time::Instant::now()
        })
        .await
        .expect("retirements never completed");
        reconciler.abort();
        let _ = reconciler.await;
        agent.abort();
        let _ = agent.await;
        server.abort();
        let _ = server.await;

        let retirements = retirements.lock().unwrap().clone();
        let mut names: Vec<_> = retirements.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        assert_eq!(
            names, apps,
            "each retirement must succeed on its first attempt"
        );
        let first = retirements.iter().map(|(_, at)| *at).min().unwrap();
        let elapsed = retired - first;
        assert!(
            elapsed < grace * 2,
            "retirements serialised: {elapsed:?} for three {grace:?} stops"
        );
    }

    /// A stand-in leader whose placement answer a test rewrites as it goes,
    /// counting every lease-retirement acknowledgement it receives.
    struct ScriptedLeader {
        assignments: Arc<std::sync::Mutex<NodeAssignments>>,
        acknowledgements: Arc<std::sync::atomic::AtomicUsize>,
        address: std::net::SocketAddr,
        server: tokio::task::JoinHandle<()>,
    }

    impl ScriptedLeader {
        async fn serve(initial: NodeAssignments) -> Self {
            let assignments = Arc::new(std::sync::Mutex::new(initial));
            let acknowledgements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let served = assignments.clone();
            let counted = acknowledgements.clone();
            let router = axum::Router::new()
                .route(
                    "/v1/placements/worker",
                    axum::routing::get(move || {
                        let current = served.lock().unwrap().clone();
                        async move { axum::Json(current) }
                    }),
                )
                .route(
                    "/v1/test/leases/retired",
                    axum::routing::post(move || {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async { axum::http::StatusCode::NO_CONTENT }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self {
                assignments,
                acknowledgements,
                address,
                server,
            }
        }

        fn assign(&self, assignments: NodeAssignments) {
            *self.assignments.lock().unwrap() = assignments;
        }

        fn acknowledged(&self) -> usize {
            self.acknowledgements
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Answer the reconciler's inventory and discovery requests as a healthy
    /// agent does, and hand every other command back to the test.
    fn answer_housekeeping(command: AgentCommand) -> Option<AgentCommand> {
        match command {
            AgentCommand::Status { response } => {
                let _ = response.send(vec![]);
                None
            }
            AgentCommand::SyncClusterConsumer { response, .. } => {
                let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                    published: true,
                    receipts: vec![],
                    requires_allocations: false,
                }));
                None
            }
            other => Some(other),
        }
    }

    fn assigned(name: &str, namespace: &str, toml: &str) -> NodeAssignment {
        NodeAssignment {
            name: name.into(),
            namespace: namespace.into(),
            ordinals: vec![0],
            spec: spec_from_toml(toml),
        }
    }

    fn deployed_app(config: &Config) -> String {
        config.app.keys().next().cloned().unwrap_or_default()
    }

    const SLOW_APP: &str =
        "[app.slow]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]";

    /// V02 (#251 follow-up): a placement that had left this node waited for
    /// every deploy of the same cycle, each allowed five minutes, before it
    /// was retired. Retirements now run first: they only cover placements
    /// already gone from this node, so nothing loses availability.
    #[tokio::test]
    async fn a_retirement_due_at_the_start_of_a_cycle_does_not_wait_for_its_deploys() {
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![assigned("slow", "default", SLOW_APP)],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        crate::cluster::applied::save(
            &checkpoint,
            &[(
                ("departed".to_string(), "default".to_string()),
                AssignmentState::Pending,
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);

        // The deploy never reaches a terminal event while the test runs.
        let mut held_deploys = Vec::new();
        let mut order = Vec::new();
        let observed = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let Some(command) = answer_housekeeping(received.recv().await.unwrap()) else {
                    continue;
                };
                match command {
                    AgentCommand::Deploy { config, events } => {
                        order.push(format!("deploy {}", deployed_app(&config)));
                        held_deploys.push(events);
                    }
                    AgentCommand::Retire {
                        app_name, response, ..
                    } => {
                        order.push(format!("retire {app_name}"));
                        let _ = response.send(Ok(()));
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        let retired = tokio::time::timeout(Duration::from_secs(2), async {
            while crate::cluster::applied::load(&checkpoint)
                .unwrap()
                .keys()
                .any(|(name, _)| name == "departed")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        leader.server.abort();

        assert!(
            observed.is_ok(),
            "the retirement waited behind a deploy: {order:?}"
        );
        assert_eq!(order.first().map(String::as_str), Some("retire departed"));
        assert!(retired.is_ok(), "the retirement was never recorded");
    }

    /// #309: the leader commits an app's placement and its catalogue
    /// allocation in separate council writes. When the catalogue write stalls
    /// or is refused ("endpoint publication generation changed") the node's
    /// answer places the app but its catalogue doesn't name the service yet.
    /// The agent then refuses the deploy with "local service requires its
    /// committed cluster allocation" and the app backs off. The reconciler
    /// now waits for the allocation instead of spending a deploy on it.
    #[tokio::test]
    async fn a_placement_waits_for_its_committed_allocation_before_deploying() {
        const WEB: &str = "[app.web]\nimage = \"proc-grill:image-ignored\"\n\
                           command = [\"sleep\", \"60\"]\nport = 8080";
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![assigned("web", "default", WEB)],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);
        let deploys = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = deploys.clone();
        let agent = tokio::spawn(async move {
            while let Some(command) = received.recv().await {
                // An enrolled consumer, which registers services only at
                // their committed allocation.
                if let AgentCommand::SyncClusterConsumer { response, .. } = command {
                    let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                        published: true,
                        receipts: vec![],
                        requires_allocations: true,
                    }));
                    continue;
                }
                let Some(command) = answer_housekeeping(command) else {
                    continue;
                };
                if let AgentCommand::Deploy { events, .. } = command {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: "cluster discovery publication failed: local service \
                                      requires its committed cluster allocation"
                                .into(),
                        })
                        .await;
                }
            }
        });

        // Two full reconcile cycles with the allocation still missing.
        tokio::time::sleep(RECONCILE_INTERVAL * 2 + Duration::from_millis(500)).await;
        let early = deploys.load(std::sync::atomic::Ordering::SeqCst);

        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let catalog = crate::onion::catalog::EndpointCatalog::new()
            .reconcile(vec![(service, 8080, vec![])])
            .unwrap();
        leader.assign(NodeAssignments {
            apps: vec![assigned("web", "default", WEB)],
            endpoint_generation: 1,
            endpoint_catalog: catalog,
            ..Default::default()
        });
        let deployed = tokio::time::timeout(RECONCILE_INTERVAL * 3, async {
            while deploys.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        agent.abort();
        leader.server.abort();

        assert_eq!(
            early, 0,
            "deployed before the catalogue allocated the service"
        );
        assert!(
            deployed.is_ok(),
            "never deployed once the allocation arrived"
        );
    }

    /// An agent without discovery ownership allocates services locally, so
    /// its placements never wait for a catalogue that may not come (the
    /// cluster tests run workers like this, with no scheduler on the leader).
    #[tokio::test]
    async fn a_placement_deploys_at_once_when_the_agent_allocates_locally() {
        const WEB: &str = "[app.web]\nimage = \"proc-grill:image-ignored\"\n\
                           command = [\"sleep\", \"60\"]\nport = 8080";
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![assigned("web", "default", WEB)],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);
        let deployed = tokio::time::timeout(RECONCILE_INTERVAL * 3, async {
            loop {
                let Some(command) = answer_housekeeping(received.recv().await.unwrap()) else {
                    continue;
                };
                if matches!(command, AgentCommand::Deploy { .. }) {
                    break;
                }
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        leader.server.abort();
        assert!(deployed.is_ok(), "waited for an allocation it doesn't need");
    }

    /// A lease released while this node is still waiting on a deploy retires
    /// within its own bound, not after the deploy: the reconciler keeps
    /// polling while it waits, and retires what those polls say has left.
    #[tokio::test]
    async fn a_released_lease_retires_while_a_deploy_is_still_in_flight() {
        const LEASED_APP: &str =
            "[app.web]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]";
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![
                assigned("slow", "default", SLOW_APP),
                assigned("web", "rbtest-run1", LEASED_APP),
            ],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        crate::cluster::applied::save(
            &checkpoint,
            &[(
                ("web".to_string(), "rbtest-run1".to_string()),
                AssignmentState::Pending,
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);

        let mut held_deploys = Vec::new();
        let mut released_at = None;
        let retired = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let Some(command) = answer_housekeeping(received.recv().await.unwrap()) else {
                    continue;
                };
                match command {
                    AgentCommand::Deploy { config, events } => {
                        assert_eq!(deployed_app(&config), "slow", "only slow may deploy");
                        held_deploys.push(events);
                        // The test run ends and releases its lease.
                        leader.assign(NodeAssignments {
                            apps: vec![assigned("slow", "default", SLOW_APP)],
                            retirements: vec![
                                serde_json::from_value(serde_json::json!({
                                    "lease_id": "run1",
                                    "placement": {
                                        "app_id": {"name": "web", "namespace": "rbtest-run1"},
                                        "node_id": "worker"
                                    }
                                }))
                                .unwrap(),
                            ],
                            ..Default::default()
                        });
                        released_at = Some(std::time::Instant::now());
                    }
                    AgentCommand::RetireTestResources {
                        app_name,
                        namespace,
                        response,
                    } => {
                        assert_eq!(
                            (app_name.as_str(), namespace.as_str()),
                            ("web", "rbtest-run1")
                        );
                        let _ = response.send(Ok(()));
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        let acknowledged = tokio::time::timeout(Duration::from_secs(4), async {
            while leader.acknowledged() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let elapsed = released_at.map(|at| at.elapsed());
        reconciler.abort();
        let _ = reconciler.await;
        leader.server.abort();

        assert!(
            retired.is_ok(),
            "the lease retirement waited behind the deploy"
        );
        assert!(
            acknowledged.is_ok(),
            "the lease retirement was never acknowledged"
        );
        assert!(
            !held_deploys.is_empty() && held_deploys.iter().all(|events| !events.is_closed()),
            "the deploy must still be in flight when the lease retires"
        );
        let elapsed = elapsed.unwrap();
        assert!(
            elapsed < lease_retirement_bound(Duration::from_secs(2)),
            "lease cleanup took {elapsed:?}"
        );
    }

    /// Retiring while a deploy is in flight must still leave one writer per
    /// app. A volume app replaced on this node (stop-first, #267) that then
    /// leaves the node is retired only once its own deploy has finished, and
    /// its return is deployed only once that retirement has answered.
    #[tokio::test]
    async fn a_volume_app_never_has_a_deploy_and_a_retirement_outstanding_at_once() {
        const VOLUME_APP: &str = "[app.db]\nimage = \"proc-grill:image-ignored\"\n\
                                  command = [\"sleep\", \"60\"]\n\
                                  [[app.db.volumes]]\npath = \"/data\"\nsize = \"1Gi\"";
        const VOLUME_APP_V2: &str = "[app.db]\nimage = \"proc-grill:image-ignored\"\n\
                                     command = [\"sleep\", \"61\"]\n\
                                     [[app.db.volumes]]\npath = \"/data\"\nsize = \"1Gi\"";
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![assigned("db", "default", VOLUME_APP)],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);

        // The one mutation of `db` the stand-in agent is carrying out.
        let writer: Arc<std::sync::Mutex<Option<&'static str>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut history = Vec::new();
        let finished = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let Some(command) = answer_housekeeping(received.recv().await.unwrap()) else {
                    continue;
                };
                match command {
                    AgentCommand::Deploy { config, events } => {
                        assert_eq!(deployed_app(&config), "db");
                        let outstanding = *writer.lock().unwrap();
                        assert_eq!(outstanding, None, "a deploy overlapped a {outstanding:?}");
                        history.push("deploy");
                        if history.len() > 1 {
                            break;
                        }
                        *writer.lock().unwrap() = Some("deploy");
                        // The app leaves this node while its deploy runs,
                        // for longer than one reconcile interval.
                        leader.assign(NodeAssignments::default());
                        let writer = writer.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(RECONCILE_INTERVAL + Duration::from_millis(500))
                                .await;
                            *writer.lock().unwrap() = None;
                            let _ = events
                                .send(ApplyEvent::Complete {
                                    created: 1,
                                    instances: vec!["default__db-0".into()],
                                })
                                .await;
                        });
                    }
                    AgentCommand::Retire {
                        app_name, response, ..
                    } => {
                        assert_eq!(app_name, "db");
                        let outstanding = *writer.lock().unwrap();
                        assert_eq!(
                            outstanding, None,
                            "a retirement overlapped a {outstanding:?}"
                        );
                        history.push("retire");
                        *writer.lock().unwrap() = Some("retirement");
                        // The app comes back, changed, while its stop runs.
                        leader.assign(NodeAssignments {
                            apps: vec![assigned("db", "default", VOLUME_APP_V2)],
                            ..Default::default()
                        });
                        let writer = writer.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            *writer.lock().unwrap() = None;
                            let _ = response.send(Ok(()));
                        });
                    }
                    _ => {}
                }
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        leader.server.abort();

        assert!(finished.is_ok(), "the app never came back: {history:?}");
        assert_eq!(history, ["deploy", "retire", "deploy"]);
    }

    /// A poll taken while a deploy runs can withdraw a placement the cycle
    /// was still going to deploy. Deploying it from the older answer would
    /// start a volume app on a node the leader has already moved it from.
    #[tokio::test]
    async fn a_placement_withdrawn_while_a_deploy_runs_is_not_deployed_from_the_older_answer() {
        const MOVED_APP: &str = "[app.db]\nimage = \"proc-grill:image-ignored\"\n\
                                 command = [\"sleep\", \"60\"]\n\
                                 [[app.db.volumes]]\npath = \"/data\"\nsize = \"1Gi\"";
        let leader = ScriptedLeader::serve(NodeAssignments {
            apps: vec![
                assigned("slow", "default", SLOW_APP),
                assigned("db", "default", MOVED_APP),
            ],
            ..Default::default()
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(leader.address, root.path(), commands);

        let mut deployed = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3) + RECONCILE_INTERVAL * 2, async {
            loop {
                let Some(command) = answer_housekeeping(received.recv().await.unwrap()) else {
                    continue;
                };
                let AgentCommand::Deploy { config, events } = command else {
                    continue;
                };
                let app = deployed_app(&config);
                deployed.push(app.clone());
                if app != "slow" {
                    continue;
                }
                leader.assign(NodeAssignments {
                    apps: vec![assigned("slow", "default", SLOW_APP)],
                    ..Default::default()
                });
                tokio::spawn(async move {
                    tokio::time::sleep(RECONCILE_INTERVAL + Duration::from_millis(500)).await;
                    let _ = events
                        .send(ApplyEvent::Complete {
                            created: 1,
                            instances: vec!["default__slow-0".into()],
                        })
                        .await;
                });
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        leader.server.abort();

        assert_eq!(deployed, ["slow"], "a withdrawn placement was deployed");
    }

    /// V02 soak (final tier, setup): after every node's agent restarted, the
    /// leader briefly moved `frontend` off node 2 and then placed it back with
    /// the same spec. The retirement in between stopped the replica but could
    /// not release its address yet ("other nodes have not yet confirmed the
    /// endpoint's withdrawal"), so the replica stayed stopped. The checkpoint
    /// still said the assignment was applied, so the returning placement was
    /// skipped as already converged, and the app ran 2 of 3 replicas for good.
    #[tokio::test]
    async fn a_placement_returning_after_a_failed_retirement_is_deployed_again() {
        let spec = spec_from_toml(
            "[app.web]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]",
        );
        let assignment = NodeAssignment {
            name: "web".into(),
            namespace: "default".into(),
            ordinals: vec![0],
            spec: spec.clone(),
        };
        let mut applied_spec = spec;
        applied_spec.replicas = Replicas::Fixed(1);
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        crate::cluster::applied::save(
            &checkpoint,
            &[(
                ("web".to_string(), "default".to_string()),
                AssignmentState::Applied {
                    fingerprint: serde_json::to_string(&applied_spec).unwrap(),
                },
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();

        // The leader withdraws the assignment until the node has tried to
        // retire it, then hands the identical assignment back.
        let returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let serve_returned = returned.clone();
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let returned = serve_returned.load(std::sync::atomic::Ordering::SeqCst);
                let assignment = assignment.clone();
                async move {
                    axum::Json(NodeAssignments {
                        apps: if returned { vec![assignment] } else { vec![] },
                        ..Default::default()
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);

        let redeployed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match received.recv().await.unwrap() {
                    AgentCommand::Status { response } => {
                        // The replica runs when the restarted reconciler checks.
                        let _ = response.send(vec![crate::bun::agent::InstanceStatus {
                            id: "default__web-0".into(),
                            app_name: "web".into(),
                            namespace: "default".into(),
                            state: "running".into(),
                            restart_count: 0,
                            host_port: Some(30000),
                            exit_code: None,
                            pid: Some(1),
                            runtime_unknown: false,
                            status_age_ms: None,
                        }]);
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Retire { response, .. } => {
                        // The stop went through; the address release did not.
                        returned.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = response.send(Err(crate::bun::BunError::ProducerReleasePending {
                            instance_id: crate::grill::InstanceId("default__web-0".into()),
                            reason: "other nodes have not yet confirmed the endpoint's withdrawal",
                        }));
                    }
                    AgentCommand::Deploy { .. } => break,
                    _ => {}
                }
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
        assert!(
            redeployed.is_ok(),
            "the returning placement was skipped as already converged"
        );
    }

    #[tokio::test]
    async fn placement_deployment_waits_for_confirmed_cluster_publication() {
        let root = tempfile::tempdir().unwrap();
        let assignments = NodeAssignments {
            endpoint_generation: 7,
            apps: vec![NodeAssignment {
                name: "web".into(),
                namespace: "default".into(),
                ordinals: vec![0],
                spec: spec_from_toml(
                    "[app.web]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]",
                ),
            }],
            ..Default::default()
        };
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let assignments = assignments.clone();
                async move { axum::Json(assignments) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);
        let pending = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match received.recv().await.unwrap() {
                    command @ AgentCommand::SyncClusterConsumer { .. } => break command,
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    AgentCommand::Deploy { .. } => {
                        panic!("deployment preceded cluster publication")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), received.recv())
                .await
                .is_err(),
            "queueing publication allowed deployment before its result"
        );
        let AgentCommand::SyncClusterConsumer {
            generation,
            response,
            ..
        } = pending
        else {
            unreachable!()
        };
        assert_eq!(generation, 7);
        response
            .send(Err(crate::bun::BunError::ClusterPublication(
                "injected refusal".into(),
            )))
            .unwrap();
        for accept in [false, true] {
            let response = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match received.recv().await.unwrap() {
                        AgentCommand::SyncClusterConsumer { response, .. } => break response,
                        AgentCommand::Status { response } => {
                            response.send(vec![]).unwrap();
                        }
                        AgentCommand::Deploy { .. } => {
                            panic!("deployment bypassed refused or lost publication")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            if accept {
                response
                    .send(Ok(crate::bun::agent::ConsumerUpdate {
                        published: true,
                        receipts: vec![],
                        requires_allocations: false,
                    }))
                    .unwrap();
            } else {
                drop(response);
            }
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match received.recv().await.unwrap() {
                    AgentCommand::Deploy { .. } => break,
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn pending_deployment_does_not_block_consumer_updates() {
        let root = tempfile::tempdir().unwrap();
        let assignments = NodeAssignments {
            endpoint_generation: 7,
            apps: vec![NodeAssignment {
                name: "web".into(),
                namespace: "default".into(),
                ordinals: vec![0],
                spec: spec_from_toml(
                    "[app.web]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"60\"]",
                ),
            }],
            ..Default::default()
        };
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let assignments = assignments.clone();
                async move { axum::Json(assignments) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);
        let mut deployment = None;
        let outcome = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                match received.recv().await.unwrap() {
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        response
                            .send(Ok(crate::bun::agent::ConsumerUpdate {
                                published: deployment.is_none(),
                                receipts: vec![],
                                requires_allocations: false,
                            }))
                            .unwrap();
                        if deployment.is_some() {
                            break;
                        }
                    }
                    AgentCommand::Deploy { events, .. } => {
                        assert!(
                            deployment.is_none(),
                            "duplicate deployment while original is pending"
                        );
                        deployment = Some(events);
                    }
                    AgentCommand::AdoptedPlacementMatches { response, .. } => {
                        let _ = response.send(false);
                    }
                    _ => panic!("unexpected command"),
                }
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
        assert!(
            outcome.is_ok(),
            "pending deployment starved consumer updates"
        );
    }

    #[test]
    fn placements_require_publication_generation_and_withdrawal_instructions() {
        for field in [
            "endpoint_generation",
            "endpoint_withdrawals",
            "endpoint_catalog",
        ] {
            let mut wire = serde_json::to_value(NodeAssignments::default()).unwrap();
            wire.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<NodeAssignments>(wire).is_err(),
                "missing {field} must not be interpreted as an empty/current publication"
            );
        }
    }

    #[tokio::test]
    async fn leased_retirement_without_a_journal_waits_for_runtime_and_persistence() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let retirement = serde_json::json!({
            "lease_id": "run1",
            "placement": {"app_id": {"name": "web", "namespace": "rbtest-run1"}, "node_id": "worker"}
        });
        let expected = retirement.clone();
        let (ack_tx, mut ack_rx) = mpsc::channel(1);
        let count = acknowledgements.clone();
        let persisted = checkpoint.clone();
        let router = axum::Router::new()
            .route(
                "/v1/placements/worker",
                axum::routing::get(move || {
                    let retirement = retirement.clone();
                    async move {
                        axum::Json(NodeAssignments {
                            retirements: vec![serde_json::from_value(retirement).unwrap()],
                            ..NodeAssignments::default()
                        })
                    }
                }),
            )
            .route(
                "/v1/test/leases/retired",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let expected = expected.clone();
                    let checkpoint = persisted.clone();
                    let count = count.clone();
                    let ack_tx = ack_tx.clone();
                    async move {
                        assert_eq!(body, expected);
                        assert!(
                            crate::cluster::applied::load(&checkpoint)
                                .unwrap()
                                .is_empty()
                        );
                        count.fetch_add(1, Ordering::SeqCst);
                        ack_tx.send(()).await.unwrap();
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);
        let mut attempts = 0;
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                tokio::select! {
                    _ = ack_rx.recv() => break,
                    command = received.recv() => match command.unwrap() {
                        AgentCommand::Status { response } => { response.send(vec![]).unwrap(); }
                        AgentCommand::SyncClusterConsumer { response, .. } => { let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate { published: true, receipts: vec![], requires_allocations: false })); }
                        AgentCommand::RetireTestResources { app_name, namespace, response } => {
                            assert_eq!((app_name.as_str(), namespace.as_str()), ("web", "rbtest-run1"));
                            assert_eq!(acknowledgements.load(Ordering::SeqCst), 0);
                            attempts += 1;
                            match attempts {
                                1 => drop(response),
                                2 => { response.send(Err(crate::bun::BunError::RetirementState {
                                    instance_id: crate::grill::InstanceId("web-0".into()),
                                    reason: "injected runtime uncertainty".into(),
                                })).unwrap(); }
                                3 => {
                                    std::fs::remove_file(&checkpoint).unwrap();
                                    std::fs::create_dir(&checkpoint).unwrap();
                                    response.send(Ok(())).unwrap();
                                }
                                4 => {
                                    std::fs::remove_dir(&checkpoint).unwrap();
                                    response.send(Ok(())).unwrap();
                                }
                                _ => panic!("retirement did not complete after repair"),
                            }
                        }
                        _ => panic!("unexpected mutation during retirement"),
                    }
                }
            }
        }).await;
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
        assert!(
            outcome.is_ok(),
            "retirement instruction was ignored or acknowledged before confirmation"
        );
        assert_eq!(attempts, 4);
        assert_eq!(acknowledgements.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconciliation_retries_a_stalled_placement_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, mut observed) = mpsc::channel(4);
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let requests = requests.clone();
                connections.spawn(async move {
                    let mut request = [0; 4096];
                    assert!(socket.read(&mut request).await.unwrap() > 0);
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{").await.unwrap();
                    requests.send(()).await.unwrap();
                    std::future::pending::<()>().await;
                });
            }
        });
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);
        let AgentCommand::Status { response } = received.recv().await.unwrap() else {
            panic!("expected recovery inventory");
        };
        response.send(vec![]).unwrap();
        tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .unwrap()
            .unwrap();
        let retried = tokio::time::timeout(Duration::from_secs(12), observed.recv()).await;
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
        assert!(
            retried.is_ok(),
            "a stalled body prevented the next placement poll"
        );
    }

    #[tokio::test]
    async fn reconciliation_retires_other_owners_after_an_agent_reply_stalls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(|| async { axum::Json(NodeAssignments::default()) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        let blocked = ("a-stalled".into(), "default".into());
        let ready = ("b-ready".into(), "default".into());
        crate::cluster::applied::save(
            &checkpoint,
            &BTreeMap::from([
                (blocked.clone(), AssignmentState::Pending),
                (ready.clone(), AssignmentState::Pending),
            ]),
        )
        .unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let reconciler = reconciler_for_deadline_test(address, root.path(), commands);
        let mut withheld = None;
        let progressed = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                match received.recv().await.unwrap() {
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Retire {
                        app_name, response, ..
                    } if app_name == blocked.0 => {
                        withheld = Some(response);
                    }
                    AgentCommand::Retire {
                        app_name, response, ..
                    } => {
                        assert_eq!(app_name, ready.0);
                        response.send(Ok(())).unwrap();
                        break;
                    }
                    _ => {}
                }
            }
            loop {
                let owned = crate::cluster::applied::load(&checkpoint).unwrap();
                assert!(owned.contains_key(&blocked));
                if !owned.contains_key(&ready) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        reconciler.abort();
        let _ = reconciler.await;
        server.abort();
        let _ = server.await;
        drop(withheld);
        assert!(
            progressed.is_ok(),
            "a stalled retirement blocked other owned resources"
        );
    }

    #[tokio::test]
    async fn a_failed_deploy_is_not_retried_on_the_next_poll() {
        // V02 bug 3: a failing app was redeployed on every 2 s poll.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let assignments = NodeAssignments {
            apps: vec![NodeAssignment {
                name: "broken".into(),
                namespace: "default".into(),
                ordinals: vec![0],
                spec: spec_from_toml(
                    r#"[app.broken]
image = "proc-grill:image-ignored"
command = ["false"]
"#,
                ),
            }],
            ..NodeAssignments::default()
        };
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let assignments = assignments.clone();
                async move { axum::Json(assignments) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (_metrics, metrics_rx) = watch::channel(openraft::RaftMetrics::new_initial(1));
        let (_directory, directory_rx) = watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: NodeId::new("leader"),
                term: 1,
                recovery_epoch: 0,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        let root = tempfile::tempdir().unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let reconciler = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx,
            directory_rx,
            0,
            None,
            commands,
            shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        let mut deploys = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(4500), async {
            while let Some(command) = received.recv().await {
                match command {
                    AgentCommand::Status { response } => {
                        let _ = response.send(vec![]);
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Deploy { events, .. } => {
                        deploys.push(std::time::Instant::now());
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: "replacement exited before its identity was recorded"
                                    .into(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }
        })
        .await;
        shutdown.cancel();
        reconciler.await.unwrap();
        server.abort();
        assert_eq!(
            deploys.len(),
            1,
            "a failed deploy was retried before its backoff elapsed"
        );
    }

    /// PR #267: a Bun upgraded between queueing the writer's deploy and
    /// recording it applied came back to a pending placement whose instances
    /// it had adopted, and rolled them anyway. When the agent says its adopted
    /// instances already run the placement, the reconciler records it applied
    /// and sends no deploy.
    #[tokio::test]
    async fn pending_placement_run_by_adopted_instances_is_recorded_without_a_redeploy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let assignments = NodeAssignments {
            apps: vec![NodeAssignment {
                name: "writer".into(),
                namespace: "default".into(),
                ordinals: vec![0],
                spec: spec_from_toml(
                    r#"[app.writer]
image = "busybox:latest"
"#,
                ),
            }],
            ..NodeAssignments::default()
        };
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let assignments = assignments.clone();
                async move { axum::Json(assignments) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (_metrics, metrics_rx) = watch::channel(openraft::RaftMetrics::new_initial(1));
        let (_directory, directory_rx) = watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: NodeId::new("leader"),
                term: 1,
                recovery_epoch: 0,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        let key = ("writer".to_string(), "default".to_string());
        crate::cluster::applied::save(
            &checkpoint,
            &AppliedMap::from([(key.clone(), AssignmentState::Pending)]),
        )
        .unwrap();
        let (commands, mut received) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let reconciler = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx,
            directory_rx,
            0,
            None,
            commands,
            shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        let running = crate::bun::agent::InstanceStatus {
            id: "default__writer-0".into(),
            app_name: "writer".into(),
            namespace: "default".into(),
            state: "running".into(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: Some(42),
            runtime_unknown: false,
            status_age_ms: None,
        };
        let mut asked = Vec::new();
        let mut deploys = 0;
        let _ = tokio::time::timeout(Duration::from_millis(4500), async {
            while let Some(command) = received.recv().await {
                match command {
                    AgentCommand::Status { response } => {
                        let _ = response.send(vec![running.clone()]);
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::AdoptedPlacementMatches {
                        app_name,
                        spec,
                        response,
                        ..
                    } => {
                        asked.push((app_name, spec.replicas));
                        let _ = response.send(true);
                    }
                    AgentCommand::Deploy { .. } => deploys += 1,
                    _ => {}
                }
            }
        })
        .await;
        shutdown.cancel();
        reconciler.await.unwrap();
        server.abort();
        assert_eq!(
            deploys, 0,
            "adopted instances that run the placement were redeployed"
        );
        assert_eq!(
            asked.first(),
            Some(&("writer".to_string(), Replicas::Fixed(1)))
        );
        let saved = crate::cluster::applied::load(&checkpoint).unwrap();
        assert!(
            matches!(saved.get(&key), Some(AssignmentState::Applied { .. })),
            "{saved:?}"
        );
    }

    #[tokio::test]
    async fn placement_ownership_is_durable_before_deployment_is_queued() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let checkpoint = crate::cluster::applied::checkpoint_path(root.path());
        let refuse_next_write = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let refusal = Arc::clone(&refuse_next_write);
        let journal = checkpoint.clone();
        let assignments = NodeAssignments {
            apps: vec![NodeAssignment {
                name: "interrupted".into(),
                namespace: "rbtest-interrupted".into(),
                ordinals: vec![0],
                spec: spec_from_toml(
                    r#"[app.interrupted]
image = "proc-grill:image-ignored"
command = ["sleep", "60"]
namespace = "rbtest-interrupted"
"#,
                ),
            }],
            ..NodeAssignments::default()
        };
        let initial = assignments.clone();
        let assignments = Arc::new(tokio::sync::RwLock::new(assignments));
        let served = Arc::clone(&assignments);
        let router = axum::Router::new().route(
            "/v1/placements/worker",
            axum::routing::get(move || {
                let served = Arc::clone(&served);
                let refusal = Arc::clone(&refusal);
                let journal = journal.clone();
                async move {
                    if refusal.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        tokio::fs::remove_file(&journal).await.unwrap();
                        tokio::fs::create_dir(&journal).await.unwrap();
                    }
                    axum::Json(served.read().await.clone())
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (_metrics, metrics_rx) = watch::channel(openraft::RaftMetrics::new_initial(1));
        let (_directory, directory_rx) = watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: NodeId::new("leader"),
                term: 1,
                recovery_epoch: 0,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });

        let (commands, mut received) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let reconciler = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx.clone(),
            directory_rx.clone(),
            0,
            None,
            commands.clone(),
            shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        let (observed, events) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match received.recv().await.unwrap() {
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Deploy { events, .. } => {
                        let saved = crate::cluster::applied::load(&checkpoint).unwrap();
                        break (saved, events);
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(
            observed.contains_key(&("interrupted".into(), "rbtest-interrupted".into())),
            "runtime work was queued before durable ownership existed"
        );

        assert_eq!(observed.values().next(), Some(&AssignmentState::Pending));
        reconciler.abort();
        assert!(reconciler.await.unwrap_err().is_cancelled());
        drop(events);
        *assignments.write().await = NodeAssignments::default();
        let restarted = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx.clone(),
            directory_rx.clone(),
            0,
            None,
            commands.clone(),
            shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        // Model a lost cleanup reply before allowing confirmed retirement.
        for attempt in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match received.recv().await.unwrap() {
                        AgentCommand::Status { response } => {
                            response.send(vec![]).unwrap();
                        }
                        AgentCommand::SyncClusterConsumer { response, .. } => {
                            let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                                published: true,
                                receipts: vec![],
                                requires_allocations: false,
                            }));
                        }
                        AgentCommand::Retire {
                            app_name,
                            namespace,
                            response,
                        } => {
                            assert_eq!(app_name, "interrupted");
                            assert_eq!(namespace, "rbtest-interrupted");
                            assert!(
                                crate::cluster::applied::load(&checkpoint)
                                    .unwrap()
                                    .contains_key(&(app_name, namespace))
                            );
                            if attempt == 1 {
                                response.send(Ok(())).unwrap();
                            }
                            break;
                        }
                        AgentCommand::Deploy { .. } => panic!("withdrawn work was redeployed"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !crate::cluster::applied::load(&checkpoint)
                .unwrap()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        restarted.await.unwrap();
        while received.try_recv().is_ok() {}

        // Fail persistence after the leader's response but before Deploy.
        *assignments.write().await = initial;
        refuse_next_write.store(true, std::sync::atomic::Ordering::SeqCst);
        let write_shutdown = CancellationToken::new();
        let write_refused = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx.clone(),
            directory_rx.clone(),
            0,
            None,
            commands.clone(),
            write_shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        tokio::time::timeout(Duration::from_secs(6), async {
            let mut polls = 0;
            while polls < 2 {
                match received.recv().await.unwrap() {
                    AgentCommand::Status { response } => {
                        response.send(vec![]).unwrap();
                    }
                    AgentCommand::SyncClusterConsumer { response, .. } => {
                        polls += 1;
                        let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate {
                            published: true,
                            receipts: vec![],
                            requires_allocations: false,
                        }));
                    }
                    AgentCommand::Deploy { .. } => {
                        panic!("deployment bypassed failed ownership persistence")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        write_shutdown.cancel();
        write_refused.await.unwrap();
        assert!(checkpoint.is_dir());
        while let Ok(command) = received.try_recv() {
            assert!(!matches!(command, AgentCommand::Deploy { .. }));
        }

        // Unreadable ownership must block new runtime work on restart.
        let refused_shutdown = CancellationToken::new();
        let refused = spawn_placement_reconciler(
            "worker".into(),
            metrics_rx,
            directory_rx,
            0,
            None,
            commands,
            refused_shutdown.clone(),
            crate::cluster::ClusterHttp::plaintext(),
            Some(root.path().to_path_buf()),
            crate::config::node::RuntimeSection::default().stop_confirmation_timeout(),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(2100), received.recv())
                .await
                .is_err()
        );
        assert!(
            !refused.is_finished(),
            "invalid ownership must retry without mutating resources"
        );
        refused_shutdown.cancel();
        refused.await.unwrap();
        server.abort();
    }

    #[test]
    fn restart_inventory_invalidates_convergence_without_forgetting_ownership() {
        let mut spec = spec_from_toml(
            r#"[app.demo]
image = "busybox:latest"
"#,
        );
        spec.replicas = Replicas::Fixed(1);
        let fingerprint = AssignmentState::Applied {
            fingerprint: serde_json::to_string(&spec).unwrap(),
        };
        let mut applied = BTreeMap::from([
            (("live".into(), "default".into()), fingerprint.clone()),
            (("gone".into(), "default".into()), fingerprint.clone()),
            (("stopped".into(), "default".into()), fingerprint),
        ]);
        let statuses = vec![
            crate::bun::agent::InstanceStatus {
                id: "live-0".into(),
                app_name: "live".into(),
                namespace: "default".into(),
                state: "running".into(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: Some(42),
                runtime_unknown: false,
                status_age_ms: None,
            },
            crate::bun::agent::InstanceStatus {
                id: "stopped-0".into(),
                app_name: "stopped".into(),
                namespace: "default".into(),
                state: "stopped".into(),
                restart_count: 0,
                host_port: None,
                exit_code: Some(0),
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
            },
        ];
        retain_live_assignments(&mut applied, &statuses);
        assert!(matches!(
            applied[&("live".into(), "default".into())],
            AssignmentState::Applied { .. }
        ));
        assert_eq!(
            applied[&("gone".into(), "default".into())],
            AssignmentState::Pending
        );
        assert_eq!(
            applied[&("stopped".into(), "default".into())],
            AssignmentState::Pending
        );
        retain_live_assignments(&mut applied, &[]);
        assert_eq!(applied.len(), 3);
        assert!(
            applied
                .values()
                .all(|state| *state == AssignmentState::Pending)
        );
    }

    // -- M14: reconciler deploy-wait timeout ---------------------------------

    #[tokio::test]
    async fn deploy_outcome_is_ok_on_complete() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(ApplyEvent::Complete {
            created: 1,
            instances: vec!["web-0".to_string()],
        })
        .await
        .unwrap();
        assert!(deploy_outcome(rx, Duration::from_secs(5)).await.is_ok());
    }

    #[tokio::test]
    async fn deploy_outcome_carries_the_agents_error_message() {
        let (tx, rx) = mpsc::channel(4);
        let message = "init container 0 failed for instance default__web-0: \
                       exited with code 1: runc run failed: container's cgroup is not empty";
        tx.send(ApplyEvent::Error {
            message: message.to_string(),
        })
        .await
        .unwrap();
        let error = deploy_outcome(rx, Duration::from_secs(5))
            .await
            .expect_err("an Error event must fail the deploy");
        assert!(
            matches!(&error, DeployWaitError::Failed(reason) if reason == message),
            "the agent's reason was dropped: {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("container's cgroup is not empty")
        );
    }

    #[tokio::test]
    async fn deploy_outcome_reports_a_closed_stream() {
        let (tx, rx) = mpsc::channel::<ApplyEvent>(4);
        drop(tx);
        assert!(matches!(
            deploy_outcome(rx, Duration::from_secs(5)).await,
            Err(DeployWaitError::Closed)
        ));
    }

    #[tokio::test]
    async fn deploy_outcome_times_out_on_a_hung_deploy() {
        // The sender is held open and never emits a terminal event — modelling
        // a stuck image pull / hung runtime. Without the timeout this would
        // wedge the reconcile tick forever; with it, the deploy is treated as
        // not-applied so the tick returns and retries.
        let (tx, rx) = mpsc::channel::<ApplyEvent>(4);
        let started = Instant::now();
        let result = deploy_outcome(rx, Duration::from_millis(100)).await;
        assert!(
            matches!(result, Err(DeployWaitError::TimedOut(_))),
            "a hung deploy must time out, not block: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must not block"
        );
        drop(tx);
    }

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

    fn readiness(name: &str, ready: bool) -> crate::reporting::types::NodeReadinessReport {
        crate::reporting::types::NodeReadinessReport {
            node_id: NodeId::new(name),
            evidence: crate::bun::readiness::NodeReadinessEvidence {
                ready,
                observed_at_unix_ms: 1,
                subsystems: vec![crate::bun::readiness::SubsystemEvidence {
                    name: "agent".to_string(),
                    critical: true,
                    state: if ready {
                        crate::bun::readiness::SubsystemState::Ready
                    } else {
                        crate::bun::readiness::SubsystemState::Degraded
                    },
                    state_since_unix_ms: 1,
                    last_error: None,
                    last_error_unix_ms: None,
                    restart_count: 0,
                }],
            },
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
            capabilities: HashMap::new(),
            readiness: HashMap::new(),
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
        assert!(!node.ready, "missing readiness evidence must fail closed");

        reports
            .readiness
            .insert(NodeId::new("a"), readiness("a", true));
        assert!(
            build_cluster_cache(&members, &reports)
                .get_node(&NodeId::new("a"))
                .unwrap()
                .ready
        );
    }

    #[test]
    fn cache_preserves_live_egress_capability() {
        let members = vec![member("guarded", 1)];
        let guarded = report(4000, 0);
        let capability = crate::reporting::types::NodeCapabilityReport {
            node_id: NodeId::new("guarded"),
            capabilities: crate::meat::cluster_state::NodeCapabilities {
                egress: crate::sesame::egress::EgressEnforcementCapability {
                    connect_ipv4: true,
                    connect_ipv6: true,
                    udp_ipv4: true,
                    udp_ipv6: true,
                    pre_start: true,
                },
                dns: Default::default(),
            },
            egress_enforcement: Vec::new(),
            egress_degraded: false,
            egress_affected_workloads: Vec::new(),
        };
        let reports = AggregatedState {
            reports: HashMap::from([(NodeId::new("guarded"), guarded)]),
            stale_nodes: vec![],
            capabilities: HashMap::from([(NodeId::new("guarded"), capability)]),
            readiness: HashMap::new(),
        };

        let cache = build_cluster_cache(&members, &reports);

        assert!(
            cache
                .get_node(&NodeId::new("guarded"))
                .unwrap()
                .capabilities
                .egress
                .can_enforce_allowlist()
        );
    }

    #[test]
    fn unenforced_running_workload_degrades_node_readiness() {
        let members = vec![member("unsafe", 1)];
        let mut unsafe_report = report(4000, 0);
        unsafe_report
            .running_apps
            .push(running_app("prod", "payer", 8080, true));
        let capability = crate::reporting::types::NodeCapabilityReport {
            node_id: NodeId::new("unsafe"),
            capabilities: Default::default(),
            egress_enforcement: vec![crate::reporting::types::EgressEnforcementEvidence {
                app_name: "payer".to_string(),
                namespace: "prod".to_string(),
                instance_id: 0,
                status: crate::reporting::types::EgressEnforcementStatus::Unenforced,
            }],
            egress_degraded: true,
            egress_affected_workloads: vec![crate::reporting::types::EgressAffectedWorkload {
                app_name: "payer".to_string(),
                namespace: "prod".to_string(),
            }],
        };
        let reports = AggregatedState {
            reports: HashMap::from([(NodeId::new("unsafe"), unsafe_report)]),
            stale_nodes: vec![],
            capabilities: HashMap::from([(NodeId::new("unsafe"), capability)]),
            readiness: HashMap::new(),
        };

        let cache = build_cluster_cache(&members, &reports);

        assert!(!cache.get_node(&NodeId::new("unsafe")).unwrap().ready);
    }

    #[test]
    fn cache_skips_zero_capacity_reports() {
        let members = vec![member("zero", 1)];
        let mut reports = AggregatedState {
            reports: HashMap::new(),
            stale_nodes: vec![],
            capabilities: HashMap::new(),
            readiness: HashMap::new(),
        };
        reports.reports.insert(NodeId::new("zero"), report(0, 0));

        let cache = build_cluster_cache(&members, &reports);
        assert_eq!(cache.node_count(), 0);
    }

    fn spec_from_toml(body: &str) -> AppSpec {
        let config = crate::config::Config::parse(body).unwrap();
        config.app.into_values().next().unwrap()
    }

    fn running_app(
        namespace: &str,
        name: &str,
        host_port: u16,
        healthy: bool,
    ) -> crate::reporting::types::RunningApp {
        use crate::reporting::types::{AppResourceUsage, ReportHealthStatus, RunningApp};
        RunningApp {
            execution: None,
            app_name: name.to_string(),
            namespace: namespace.to_string(),
            instance_id: 0,
            image: "x:1".to_string(),
            port: Some(host_port),
            health_status: if healthy {
                ReportHealthStatus::Healthy
            } else {
                ReportHealthStatus::Starting
            },
            uptime: Duration::from_secs(1),
            resource_usage: AppResourceUsage::default(),
        }
    }

    #[test]
    fn retired_vips_are_reserved_for_departing_and_previously_withdrawn_services() {
        use crate::onion::{catalog::EndpointCatalog, service_id::ServiceId, vip::VirtualIP};
        let mut original =
            EndpointCatalog::rebuild([(ServiceId::new("default", "old"), 80, vec![])]).unwrap();
        let reserved = VirtualIP::from_service_id(&ServiceId::new("default", "new"));
        original.services.get_mut("default__old").unwrap().vip = reserved;
        for already_withdrawn in [false, true] {
            let mut desired = crate::council::types::DesiredState::default();
            desired.endpoint_consumers.insert("reader".into());
            desired.endpoint_withdrawals = desired
                .endpoint_withdrawals
                .plan_publication(
                    &EndpointCatalog::default(),
                    &original,
                    &desired.endpoint_consumers,
                )
                .unwrap();
            if already_withdrawn {
                desired.endpoint_withdrawals = desired
                    .endpoint_withdrawals
                    .plan_publication(
                        &original,
                        &EndpointCatalog::default(),
                        &desired.endpoint_consumers,
                    )
                    .unwrap();
            } else {
                desired.endpoint_catalog = original.clone();
            }
            desired.apps.insert(
                crate::meat::types::AppId::new("new", "default"),
                spec_from_toml("[app.new]\nimage = \"x:1\"\nport = 80\n"),
            );
            let candidate =
                build_endpoint_catalog(&[], &AggregatedState::default(), &desired).unwrap();
            assert_ne!(candidate.services["default__new"].vip, reserved);
            assert!(
                desired
                    .endpoint_withdrawals
                    .plan_publication(
                        &desired.endpoint_catalog,
                        &candidate,
                        &desired.endpoint_consumers
                    )
                    .is_ok()
            );
        }
    }

    #[test]
    fn producer_scheduler_filters_retired_and_uncorrelated_reports_but_preserves_successors() {
        let mut desired = crate::council::types::DesiredState::default();
        desired.apps.insert(
            crate::meat::types::AppId::new("web", "default"),
            spec_from_toml("[app.web]\nimage = \"x:1\"\nport = 80\n"),
        );
        let execution: crate::grill::RuntimeExecution = serde_json::from_value(serde_json::json!({
            "instance_id": "default__web-0", "generation": "a".repeat(64)
        }))
        .unwrap();
        desired.producer_retirements = desired
            .producer_retirements
            .plan_retirement("a", &execution)
            .unwrap();
        let mut successor = execution.clone();
        successor.generation = "b".repeat(64).try_into().unwrap();
        for original in [None, Some(execution), Some(successor.clone())] {
            let mut report = report(4000, 0);
            let mut app = running_app("default", "web", 30001, true);
            app.execution = original.clone();
            report.running_apps = vec![app];
            let mut reports = AggregatedState::default();
            reports.reports.insert(NodeId::new("a"), report);
            let catalog = build_endpoint_catalog(&[member("a", 1)], &reports, &desired).unwrap();
            assert_eq!(
                catalog.services["default__web"].backends.len(),
                usize::from(original == Some(successor.clone()))
            );
        }
    }

    #[test]
    fn build_endpoint_catalog_aggregates_backends_across_nodes() {
        use crate::onion::service_id::ServiceId;

        // Two nodes each run one instance of the same service (`default/api`);
        // the catalogue must carry both backends with each node's real IP.
        let members = vec![member("node-a", 5001), member("node-b", 5002)];
        let mut reports = AggregatedState {
            reports: HashMap::new(),
            stale_nodes: vec![],
            capabilities: HashMap::new(),
            readiness: HashMap::new(),
        };
        let mut ra = report(4000, 100);
        ra.running_apps = vec![running_app("default", "api", 30001, true)];
        let execution = crate::grill::RuntimeExecution {
            instance_id: crate::grill::InstanceId("default__api-g3-0".into()),
            generation: crate::grill::RuntimeGeneration::process("private-runtime-generation"),
        };
        ra.running_apps[0].execution = Some(execution.clone());
        reports.reports.insert(NodeId::new("node-a"), ra);
        let mut rb = report(4000, 100);
        rb.running_apps = vec![running_app("default", "api", 30002, false)];
        reports.reports.insert(NodeId::new("node-b"), rb);

        // Declared container port comes from the desired spec.
        let mut desired = crate::council::types::DesiredState::default();
        desired.apps.insert(
            crate::meat::types::AppId::new("api", "default"),
            spec_from_toml("[app.api]\nimage = \"x:1\"\nport = 3000\n"),
        );

        let catalog = build_endpoint_catalog(&members, &reports, &desired).unwrap();
        let svc = catalog.resolve(&ServiceId::new("default", "api")).unwrap();
        assert_eq!(svc.port, 3000, "declared port taken from the spec");
        assert_eq!(svc.backends.len(), 2, "both nodes' backends present");
        assert_eq!(
            svc.backends
                .iter()
                .find(|backend| backend.node_id == "node-a")
                .unwrap()
                .execution,
            Some(execution)
        );
        assert!(
            svc.backends
                .iter()
                .any(|b| b.node_id == "node-a" && b.healthy)
        );
        assert!(
            svc.backends
                .iter()
                .any(|b| b.node_id == "node-b" && !b.healthy)
        );
    }

    /// A live node that hasn't reported under this leader keeps the backends
    /// the committed catalogue gave it. A fresh leader starts with no reports
    /// at all, and a restarted agent needs a few seconds before its first one;
    /// dropping those backends made every node's connect hook refuse live
    /// services with EPERM until the reports arrived (V02 soak).
    #[test]
    fn build_endpoint_catalog_keeps_committed_backends_of_live_unreported_nodes() {
        use crate::onion::catalog::CatalogBackend;

        let mut desired = crate::council::types::DesiredState::default();
        desired.apps.insert(
            crate::meat::types::AppId::new("redis", "default"),
            spec_from_toml("[app.redis]\nimage = \"x:1\"\nport = 6379\n"),
        );
        let execution: crate::grill::RuntimeExecution = serde_json::from_value(serde_json::json!({
            "instance_id": "default__redis-0", "generation": "a".repeat(64)
        }))
        .unwrap();
        let committed = CatalogBackend {
            execution: Some(execution.clone()),
            node_id: "node-b".into(),
            node_ip: "127.0.0.1".parse().unwrap(),
            host_port: 36555,
            healthy: true,
        };
        desired.endpoint_catalog =
            build_endpoint_catalog(&[], &AggregatedState::default(), &desired).unwrap();
        desired
            .endpoint_catalog
            .services
            .get_mut("default__redis")
            .unwrap()
            .backends = vec![committed.clone()];

        // Only the new leader has reported so far.
        let mut reports = AggregatedState::default();
        reports
            .reports
            .insert(NodeId::new("node-a"), report(4000, 0));
        let members = vec![member("node-a", 5001), member("node-b", 5002)];
        let catalog = build_endpoint_catalog(&members, &reports, &desired).unwrap();
        assert_eq!(
            catalog.services["default__redis"].backends,
            vec![committed.clone()],
            "a live node's committed backend survives until it reports"
        );

        // Suspect is still a member that may be serving.
        let mut suspect = members.clone();
        suspect[1].state = NodeState::Suspect;
        let catalog = build_endpoint_catalog(&suspect, &reports, &desired).unwrap();
        assert_eq!(catalog.services["default__redis"].backends.len(), 1);

        // Its own report is authoritative, even when it names nothing.
        let mut reported = reports.clone();
        reported
            .reports
            .insert(NodeId::new("node-b"), report(4000, 0));
        let catalog = build_endpoint_catalog(&members, &reported, &desired).unwrap();
        assert!(catalog.services["default__redis"].backends.is_empty());

        // A dead, departed or re-addressed node can't be serving there.
        for gone in [Some(NodeState::Dead), Some(NodeState::Left), None] {
            let mut members = members.clone();
            match gone {
                Some(state) => members[1].state = state,
                None => {
                    members.pop();
                }
            }
            let catalog = build_endpoint_catalog(&members, &reports, &desired).unwrap();
            assert!(
                catalog.services["default__redis"].backends.is_empty(),
                "{gone:?}"
            );
        }
        let mut moved = members.clone();
        moved[1].address = "127.0.0.2:5002".parse().unwrap();
        let catalog = build_endpoint_catalog(&moved, &reports, &desired).unwrap();
        assert!(catalog.services["default__redis"].backends.is_empty());

        // A producer retirement still withdraws it.
        let mut retiring = desired.clone();
        retiring.producer_retirements = retiring
            .producer_retirements
            .plan_retirement("node-b", &execution)
            .unwrap();
        let catalog = build_endpoint_catalog(&members, &reports, &retiring).unwrap();
        assert!(catalog.services["default__redis"].backends.is_empty());

        // A deleted app takes its service with it.
        let mut deleted = desired.clone();
        deleted.apps.clear();
        let catalog = build_endpoint_catalog(&members, &reports, &deleted).unwrap();
        assert!(
            catalog
                .services
                .get("default__redis")
                .is_none_or(|service| service.backends.is_empty())
        );
    }

    #[test]
    fn build_endpoint_catalog_skips_portless_and_unknown_apps() {
        // A running app with no host port, and one with no desired spec, are
        // both skipped — nothing to resolve.
        let members = vec![member("node-a", 5001)];
        let mut reports = AggregatedState {
            reports: HashMap::new(),
            stale_nodes: vec![],
            capabilities: HashMap::new(),
            readiness: HashMap::new(),
        };
        let mut ra = report(4000, 100);
        let mut portless = running_app("default", "batch", 0, true);
        portless.port = None;
        ra.running_apps = vec![
            portless,
            running_app("default", "orphan", 30001, true), // no spec
        ];
        reports.reports.insert(NodeId::new("node-a"), ra);

        let desired = crate::council::types::DesiredState::default();
        let catalog = build_endpoint_catalog(&members, &reports, &desired).unwrap();
        assert!(
            catalog.is_empty(),
            "portless and spec-less apps must be skipped"
        );
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
            // Two instances, labelled exactly as `mayo::collector` labels them,
            // reporting `process_cpu_percent` (percent of ONE core).
            let entry = |instance: &str, sum: f64, count: u32| {
                let mut labels = Map::new();
                labels.insert("app".to_string(), "prod/web".to_string());
                labels.insert("namespace".to_string(), "prod".to_string());
                labels.insert("instance".to_string(), instance.to_string());
                RollupEntry {
                    metric_name: "process_cpu_percent".to_string(),
                    labels,
                    aggregate: RollupAggregate {
                        min: 0.0,
                        max: sum,
                        sum,
                        count,
                    },
                }
            };
            let rollup = NodeRollup {
                node_id: NodeId::new("n1"),
                timestamp: now.saturating_sub(60),
                // Instance means: 30% and 50% of a core → 40% on average.
                entries: vec![
                    entry("prod__web-0", 60.0, 2),
                    entry("prod__web-1", 100.0, 2),
                ],
            };
            let mut w = store.write().await;
            w.ingest(&rollup);
            w.flush().await.unwrap();
        }

        // 200m requested = 20% of a core; 40% used → 2.0 utilisation.
        let spec = crate::config::app::AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "50%".to_string(),
            min: 1,
            max: 5,
            evaluation_window: Some("5m".to_string()),
            cooldown: None,
            scale_down_threshold: None,
        };
        let cpu = Some(crate::config::types::ResourceRange {
            request: 200,
            limit: 1000,
        });
        let config = crate::meat::autoscaler::AutoscaleConfig::from_spec(&spec, cpu, None).unwrap();
        let value = app_metric_utilisation(&store, &config, &AppId::new("web", "prod")).await;
        assert!(
            value.is_some_and(|v| (v - 2.0).abs() < 1e-9),
            "expected utilisation 2.0 of the request, got {value:?}"
        );

        // Same app name, different namespace → no data (M26).
        assert!(
            app_metric_utilisation(&store, &config, &AppId::new("web", "staging"))
                .await
                .is_none()
        );
        // Unknown app → no data.
        assert!(
            app_metric_utilisation(&store, &config, &AppId::new("other", "prod"))
                .await
                .is_none()
        );
        // Memory scaling reads a different series, which this store lacks.
        let memory_spec = crate::config::app::AutoscaleSpec {
            metric: "memory".to_string(),
            ..spec
        };
        let memory = Some(crate::config::types::ResourceRange {
            request: 1 << 20,
            limit: 1 << 20,
        });
        let memory_config =
            crate::meat::autoscaler::AutoscaleConfig::from_spec(&memory_spec, None, memory)
                .unwrap();
        assert!(
            app_metric_utilisation(&store, &memory_config, &AppId::new("web", "prod"))
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
            capabilities: Default::default(),
            app_replicas: Default::default(),
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

    fn placed_on(names: &[&str]) -> Vec<crate::meat::types::Placement> {
        names
            .iter()
            .zip(0..)
            .map(|(name, ordinal)| crate::meat::types::Placement {
                node_id: NodeId::new(*name),
                resources: Resources::new(100, 0, 0),
                ordinal,
            })
            .collect()
    }

    // -- cluster-wide ordinals (#398) ----------------------------------------

    /// Placements of `spec`'s size, as `(node, ordinal)` pairs.
    fn placed_as(spec: &AppSpec, placed: &[(&str, u32)]) -> Vec<crate::meat::types::Placement> {
        placed
            .iter()
            .map(|(name, ordinal)| crate::meat::types::Placement {
                node_id: NodeId::new(*name),
                resources: scheduler_resources(spec),
                ordinal: *ordinal,
            })
            .collect()
    }

    /// Each placement's ordinal, lowest first, with its node.
    fn ordinals_of(decision: &crate::meat::types::SchedulingDecision) -> Vec<(u32, &str)> {
        let mut ordinals: Vec<(u32, &str)> = decision
            .placements
            .iter()
            .map(|p| (p.ordinal, p.node_id.0.as_str()))
            .collect();
        ordinals.sort_unstable();
        ordinals
    }

    fn three_node_cache() -> (ClusterStateCache, HashSet<NodeId>) {
        let mut cache = ClusterStateCache::new();
        for name in ["n1", "n2", "n3"] {
            cache.set_node(sched_node(name, 4000, BTreeMap::new()));
        }
        let alive = ["n1", "n2", "n3"].into_iter().map(NodeId::new).collect();
        (cache, alive)
    }

    /// The tour showed `frontend-0` on all three nodes: each node counted its
    /// own replicas from 0. The leader numbers them across the cluster.
    #[test]
    fn three_replicas_on_three_nodes_take_distinct_ordinals() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 3));
        let (mut cache, alive) = three_node_cache();

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        let ordinals: Vec<u32> = ordinals_of(&decisions[0]).iter().map(|o| o.0).collect();
        assert_eq!(ordinals, [0, 1, 2]);
        let nodes: HashSet<&str> = nodes_of(&decisions[0]).into_iter().collect();
        assert_eq!(nodes.len(), 3, "{decisions:?}");
    }

    #[test]
    fn a_replacement_takes_the_ordinal_its_lost_replica_held() {
        let app = AppId::new("frontend", "default");
        let spec = app_spec(100, 3);
        let mut desired = DesiredState::default();
        desired.scheduling.insert(
            app.clone(),
            placed_as(&spec, &[("n1", 0), ("n2", 1), ("n3", 2)]),
        );
        desired.apps.insert(app.clone(), spec);
        // n2 is gone: gossip dropped it and the cache has no report of it.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n3", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        let ordinals = ordinals_of(&decisions[0]);
        assert_eq!(ordinals[0], (0, "n1"), "{ordinals:?}");
        assert_eq!(ordinals[2], (2, "n3"), "{ordinals:?}");
        assert_eq!(ordinals[1].0, 1, "{ordinals:?}");
        assert_ne!(ordinals[1].1, "n2", "{ordinals:?}");
    }

    #[test]
    fn a_scale_up_takes_the_lowest_free_ordinals() {
        let app = AppId::new("frontend", "default");
        let spec = app_spec(100, 4);
        let mut desired = DesiredState::default();
        // Ordinal 1's node was lost and decommissioned earlier.
        desired
            .scheduling
            .insert(app.clone(), placed_as(&spec, &[("n1", 0), ("n3", 2)]));
        desired.apps.insert(app.clone(), spec);
        let (mut cache, alive) = three_node_cache();

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        let ordinals = ordinals_of(&decisions[0]);
        let numbers: Vec<u32> = ordinals.iter().map(|o| o.0).collect();
        assert_eq!(numbers, [0, 1, 2, 3], "{ordinals:?}");
        assert_eq!(ordinals[0], (0, "n1"));
        assert_eq!(ordinals[2], (2, "n3"));
    }

    #[test]
    fn a_scale_down_retires_the_highest_ordinals() {
        let app = AppId::new("frontend", "default");
        let spec = app_spec(100, 2);
        let mut desired = DesiredState::default();
        // Listed out of order: the planner goes by ordinal, not by position.
        desired.scheduling.insert(
            app.clone(),
            placed_as(&spec, &[("n3", 2), ("n1", 0), ("n2", 1)]),
        );
        desired.apps.insert(app.clone(), spec);
        let (mut cache, alive) = three_node_cache();

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(ordinals_of(&decisions[0]), [(0, "n1"), (1, "n2")]);
    }

    /// A rolling deploy changes the spec, not the placements; a resize is the
    /// one spec change the leader re-plans, and it keeps every ordinal where
    /// it was.
    #[test]
    fn a_resized_app_keeps_every_ordinal_on_its_node() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.scheduling.insert(
            app.clone(),
            placed_as(&app_spec(100, 3), &[("n1", 0), ("n2", 1), ("n3", 2)]),
        );
        desired.apps.insert(app.clone(), app_spec(200, 3));
        let (mut cache, alive) = three_node_cache();

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(
            ordinals_of(&decisions[0]),
            [(0, "n1"), (1, "n2"), (2, "n3")]
        );
        assert!(
            decisions[0]
                .placements
                .iter()
                .all(|p| p.resources == scheduler_resources(&app_spec(200, 3)))
        );
    }

    /// A daemon set is re-planned whole when its eligible nodes change. The
    /// nodes it already runs on keep their ordinals; a joining node takes the
    /// lowest free one.
    #[test]
    fn a_daemon_set_keeps_each_nodes_ordinal_when_a_node_joins() {
        let app = AppId::new("agent", "default");
        let mut spec = app_spec(100, 1);
        spec.replicas = Replicas::DaemonSet;
        let mut desired = DesiredState::default();
        desired
            .scheduling
            .insert(app.clone(), placed_as(&spec, &[("n3", 0), ("n1", 1)]));
        desired.apps.insert(app.clone(), spec);
        let (mut cache, alive) = three_node_cache();

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(
            ordinals_of(&decisions[0]),
            [(0, "n3"), (1, "n1"), (2, "n2")]
        );
    }

    /// A volume app returning from a stop gets its old ordinals back on its
    /// home nodes: the leader records the nodes in ordinal order.
    #[test]
    fn a_returning_volume_app_takes_its_ordinals_in_home_order() {
        let (desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(ordinals_of(&decisions[0]), [(0, "home")]);
    }

    fn nodes_of(decision: &crate::meat::types::SchedulingDecision) -> Vec<&str> {
        decision
            .placements
            .iter()
            .map(|p| p.node_id.0.as_str())
            .collect()
    }

    /// `relish stop` keeps the spec but schedules nothing until the next apply.
    #[test]
    fn a_stopped_app_is_scheduled_at_zero_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 2));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2"]));
        desired.stopped_apps.insert(app.clone());
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1);
        assert!(nodes_of(&decisions[0]).is_empty(), "{decisions:?}");
    }

    fn app_with_volume(cpu_request: u64) -> AppSpec {
        let mut spec = app_spec(cpu_request, 1);
        spec.volumes.push(crate::config::types::VolumeSpec {
            path: "/data".into(),
            source: None,
            size: None,
        });
        spec
    }

    /// Two nodes where the scheduler, left to itself, prefers `busy`: it
    /// bin-packs onto the fuller node. The volume app last ran on `home`.
    fn stopped_and_applied_again(spec: AppSpec) -> (DesiredState, ClusterStateCache) {
        let app = AppId::new("db", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), spec);
        // `relish stop` committed an empty decision, then `apply` cleared
        // the stop mark.
        desired.scheduling.insert(app.clone(), Vec::new());
        desired
            .last_placed_nodes
            .insert(app, vec![NodeId::new("home")]);
        let mut cache = ClusterStateCache::new();
        let mut busy = sched_node("busy", 4000, BTreeMap::new());
        busy.allocated = Resources::new(3000, 0, 0);
        cache.set_node(busy);
        cache.set_node(sched_node("home", 4000, BTreeMap::new()));
        (desired, cache)
    }

    /// V02 soak on 9e6a6b6: a marker written into `vol-persist`'s managed
    /// volume was gone after `relish stop` and `apply`. The stop cleared the
    /// app's placements, so the redeploy went wherever the scheduler liked,
    /// onto a node with a new, empty volume. The data was still on the old
    /// node.
    #[test]
    fn a_volume_app_comes_back_on_the_node_that_holds_its_volume() {
        let (desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(nodes_of(&decisions[0]), ["home"]);
    }

    /// Placing it elsewhere would hand it an empty volume, so an app whose
    /// home is alive but full waits for room there instead.
    #[test]
    fn a_volume_app_waits_for_room_on_the_node_that_holds_its_volume() {
        let (desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let mut full = sched_node("home", 4000, BTreeMap::new());
        full.allocated = Resources::new(4000, 0, 0);
        cache.set_node(full);
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// Retire `node` the way `relish decommission-node` does.
    fn decommission(desired: &mut DesiredState, node: &str) {
        desired.security_state.crl.retired_nodes.insert(
            node.into(),
            crate::cluster::retirement::NodeRetirement {
                node_id: node.into(),
                retired_by: "operator".into(),
                reason: "disk died".into(),
                retired_at_unix_ms: 30,
                released_placements: Default::default(),
                released_registry_writers: Default::default(),
                released_node_fault: None,
                released_endpoint_consumer: false,
            },
        );
    }

    /// #423: a home that left gossip, went Dead, or was reaped from the
    /// member table entirely is usually a node restarting or rebooting, with
    /// its data on disk. The app waits for it rather than starting elsewhere
    /// on an empty volume.
    #[test]
    fn a_volume_app_waits_while_the_node_that_holds_its_volume_is_out_of_the_cluster() {
        let (desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let alive = HashSet::from([NodeId::new("busy")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// Decommissioning the home node is the operator saying its data is
    /// gone: that, and only that, lets the app start somewhere else.
    #[test]
    fn a_volume_app_whose_node_was_decommissioned_is_placed_elsewhere() {
        let (mut desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        decommission(&mut desired, "home");
        let alive = HashSet::from([NodeId::new("busy")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(nodes_of(&decisions[0]), ["busy"]);
    }

    /// #423: the tester's case. `systemctl restart bun` announces Left, and
    /// a running volume app's placement must survive it, Suspect and Dead
    /// alike, until the node comes back.
    #[test]
    fn a_running_volume_app_keeps_its_placement_while_its_node_restarts() {
        let (mut desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let app = AppId::new("db", "default");
        desired.scheduling.insert(app, placed_on(&["home"]));
        let alive = HashSet::from([NodeId::new("busy")]);

        for suspect in [HashSet::new(), HashSet::from([NodeId::new("home")])] {
            let decisions = plan_scheduling_pass_with_dns(
                &mut cache,
                &desired,
                &alive,
                &suspect,
                &mut QuotaLedger::default(),
                false,
                &HashSet::new(),
            );
            assert!(decisions.is_empty(), "{decisions:?}");
        }

        decommission(&mut desired, "home");
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(nodes_of(&decisions[0]), ["busy"]);
    }

    /// #423 meets #432: home has just come back from a restart and its
    /// first report doesn't list db yet. db keeps its placement, and the
    /// room it is about to take again is rebuilt from the commitment, so
    /// another app can't claim it in the meantime.
    #[test]
    fn a_restarted_home_keeps_room_for_the_volume_app_it_hasnt_reported_yet() {
        let db = AppId::new("db", "default");
        let web = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(db.clone(), app_with_volume(600));
        desired.scheduling.insert(
            db.clone(),
            vec![crate::meat::types::Placement {
                node_id: NodeId::new("home"),
                resources: Resources::new(600, 0, 0),
                ordinal: 0,
            }],
        );
        desired
            .last_placed_nodes
            .insert(db.clone(), vec![NodeId::new("home")]);
        desired.apps.insert(web.clone(), app_spec(600, 1));
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("home", 1000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.iter().all(|d| d.app_id != db), "{decisions:?}");
        assert!(decisions.iter().all(|d| d.app_id != web), "{decisions:?}");
        let home = cache.get_node(&NodeId::new("home")).unwrap();
        assert_eq!(home.allocated.cpu_millicores, 600);
    }

    /// Each replica of a volume app has its own volume on its own node, so
    /// losing one node must not start a fresh replica on an empty volume
    /// either; the others keep serving.
    #[test]
    fn a_volume_replica_on_a_departed_node_is_not_replaced() {
        let app = AppId::new("db", "default");
        let mut spec = app_with_volume(100);
        spec.replicas = Replicas::Fixed(2);
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), spec);
        desired.scheduling.insert(app, placed_on(&["n1", "n2"]));
        let mut cache = ClusterStateCache::new();
        for name in ["n1", "n3"] {
            cache.set_node(sched_node(name, 4000, BTreeMap::new()));
        }
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// Without a managed volume there's nothing to go back for.
    #[test]
    fn an_app_without_a_volume_is_placed_by_score_after_a_stop() {
        let (desired, mut cache) = stopped_and_applied_again(app_spec(100, 1));
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(nodes_of(&decisions[0]), ["busy"]);
    }

    /// V02 FINAL on ff854cb: `vol-persist` lost its marker again. Between
    /// the stop and the apply, the home node's report worker timed out for
    /// over 30 seconds. The leader kept the node's last state report but
    /// marked it stale and dropped its readiness, so the home looked "not
    /// ready" and was dropped as if it were gone. The node was alive the
    /// whole time, with the data on it.
    #[test]
    fn a_volume_app_waits_while_the_node_that_holds_its_volume_reports_stale() {
        let (desired, _) = stopped_and_applied_again(app_with_volume(100));
        let members = vec![member("busy", 1), member("home", 2)];
        let mut reports = AggregatedState::default();
        reports
            .reports
            .insert(NodeId::new("busy"), report(4000, 3000));
        reports.reports.insert(NodeId::new("home"), report(4000, 0));
        reports
            .readiness
            .insert(NodeId::new("busy"), readiness("busy", true));
        reports.stale_nodes.push(NodeId::new("home"));
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);
        let mut cache = build_cluster_cache(&members, &reports);
        let unheard = unheard_nodes(&alive, &reports);

        let decisions = plan_scheduling_pass_with_dns(
            &mut cache,
            &desired,
            &alive,
            &HashSet::new(),
            &mut QuotaLedger::default(),
            false,
            &unheard,
        );

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// A node mid-upgrade is cordoned, which the cache records as not
    /// ready. That's a reason to put nothing new on it, not to start its
    /// volume app somewhere else on an empty volume.
    #[test]
    fn a_volume_app_waits_while_the_node_that_holds_its_volume_is_not_ready() {
        let (desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let mut unready = sched_node("home", 4000, BTreeMap::new());
        unready.ready = false;
        cache.set_node(unready);
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// The same stall must not move a running volume app either: its
    /// replacement would start on an empty volume while the data sits on
    /// the node it left.
    #[test]
    fn a_running_volume_app_stays_on_its_node_while_that_node_is_not_ready() {
        let (mut desired, mut cache) = stopped_and_applied_again(app_with_volume(100));
        let app = AppId::new("db", "default");
        desired.scheduling.insert(app, placed_on(&["home"]));
        let mut unready = sched_node("home", 4000, BTreeMap::new());
        unready.ready = false;
        cache.set_node(unready);
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");

        // Gone from gossip is usually a restart (#423): it still stays.
        let alive = HashSet::from([NodeId::new("busy")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// The operator moving an app with `placement.required` is on purpose.
    #[test]
    fn a_volume_app_whose_node_no_longer_matches_its_labels_is_placed_elsewhere() {
        let mut spec = app_with_volume(100);
        spec.placement = Some(toml::from_str(r#"required = ["disk=ssd"]"#).unwrap());
        let (desired, mut cache) = stopped_and_applied_again(spec);
        let mut busy = sched_node(
            "busy",
            4000,
            BTreeMap::from([("disk".to_string(), "ssd".to_string())]),
        );
        busy.allocated = Resources::new(3000, 0, 0);
        cache.set_node(busy);
        let alive = HashSet::from([NodeId::new("busy"), NodeId::new("home")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(nodes_of(&decisions[0]), ["busy"]);
    }

    /// #211 made `relish stop` keep the spec; the app's ingress route must
    /// still go, or its host answers 503 instead of 404.
    #[test]
    fn a_stopped_app_has_no_cluster_ingress_route() {
        let mut desired = DesiredState::default();
        for name in ["kept", "stopped"] {
            let mut spec = app_spec(100, 1);
            spec.ingress = Some(toml::from_str(&format!("host = \"{name}.example\"")).unwrap());
            desired.apps.insert(AppId::new(name, "default"), spec);
        }
        desired
            .stopped_apps
            .insert(AppId::new("stopped", "default"));

        let hosts: Vec<_> = cluster_ingress(&desired)
            .into_iter()
            .map(|route| route.config.host)
            .collect();

        assert_eq!(hosts, ["kept.example"]);
    }

    /// Z6.7: stopping one laptop node moved all three frontends onto a single
    /// survivor, restarting the two that were serving fine.
    #[test]
    fn losing_a_node_replaces_only_its_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 3));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1);
        let nodes = nodes_of(&decisions[0]);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[..2], ["n1", "n2"], "survivors keep their replicas");
        assert!(["n1", "n2"].contains(&nodes[2]), "{nodes:?}");
    }

    /// A survivor's load outside this app, so the bin-packer prefers it.
    fn busier_node(name: &str, used_cpu: u64) -> SchedulerNodeState {
        let mut node = sched_node(name, 4000, BTreeMap::new());
        node.allocated = Resources::new(used_cpu, 0, 0);
        node
    }

    /// How many of a decision's placements land on each node.
    fn per_node(decision: &crate::meat::types::SchedulingDecision) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for node in nodes_of(decision) {
            *counts.entry(node).or_default() += 1;
        }
        counts
    }

    /// #346: a node running two of four replicas dies. Both survivors run
    /// the app and one is busier, so the bin-packer used to put both
    /// replacements there: three on one node, one on the other.
    #[test]
    fn replacements_spread_over_the_survivors_with_fewest_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 4));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n1", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        cache.set_node(busier_node("n2", 2000));
        cache.set_node(sched_node("n3", 4000, BTreeMap::new()));
        // Both survivors report their replica.
        for name in ["n2", "n3"] {
            cache.reserve(&NodeId::new(name), &app, &Resources::new(100, 0, 0));
        }
        let alive = HashSet::from([NodeId::new("n2"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        let nodes = nodes_of(&decisions[0]);
        assert_eq!(nodes[..2], ["n2", "n3"], "survivors keep their replicas");
        assert_eq!(
            per_node(&decisions[0]),
            BTreeMap::from([("n2", 2), ("n3", 2)]),
            "{nodes:?}"
        );
    }

    /// #346: the survivors' kept placements count towards spread even before
    /// their reports list the app (a new leader, or a report in transit).
    #[test]
    fn kept_placements_count_towards_spread_before_reports_list_them() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 4));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        cache.set_node(busier_node("n2", 2000));
        cache.set_node(sched_node("n3", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n2"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(
            per_node(&decisions[0]),
            BTreeMap::from([("n2", 2), ("n3", 2)]),
            "{:?}",
            nodes_of(&decisions[0])
        );
    }

    /// Spread gives way to resources: a survivor with no room left doesn't
    /// take a replacement, so the other one takes both.
    #[test]
    fn replacements_stack_only_when_the_other_survivor_is_full() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 4));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n1", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        cache.set_node(busier_node("n3", 3950));
        let alive = HashSet::from([NodeId::new("n2"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        // n2 and n3 keep ordinals 2 and 3; both replacements land on n2,
        // under the ordinals n1's replicas had.
        assert_eq!(
            ordinals_of(&decisions[0]),
            [(0, "n2"), (1, "n2"), (2, "n2"), (3, "n3")]
        );
    }

    /// #346: one of three nodes dies while gossip only suspects another
    /// survivor (a probe answered late on a loaded laptop). Suspicion isn't
    /// death: the suspect keeps its replica, the dead node's replica moves to
    /// a live survivor, and nothing else changes.
    #[test]
    fn a_suspect_node_keeps_its_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 3));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n3"]));
        // A suspect node isn't in the scheduler's cache: only live nodes are.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1")]);
        let suspect = HashSet::from([NodeId::new("n2")]);

        let decisions = plan_scheduling_pass_with_dns(
            &mut cache,
            &desired,
            &alive,
            &suspect,
            &mut QuotaLedger::default(),
            false,
            &HashSet::new(),
        );

        assert_eq!(nodes_of(&decisions[0]), ["n1", "n2", "n1"]);

        // With every placement on a live or suspect node, nothing moves.
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n1"]));
        let decisions = plan_scheduling_pass_with_dns(
            &mut cache,
            &desired,
            &alive,
            &suspect,
            &mut QuotaLedger::default(),
            false,
            &HashSet::new(),
        );
        assert!(decisions.is_empty(), "{decisions:?}");
    }

    /// A new leader that hasn't heard from a live node yet leaves that
    /// node's replicas alone.
    #[test]
    fn a_live_node_that_has_not_reported_yet_keeps_its_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 3));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(nodes_of(&decisions[0]), ["n1", "n2", "n1"]);

        // Reporting not ready is evidence; that node's replica moves.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        let mut unready = sched_node("n2", 4000, BTreeMap::new());
        unready.ready = false;
        cache.set_node(unready);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(nodes_of(&decisions[0]), ["n1", "n1", "n1"]);
    }

    /// Z6.7: after node-3 (the leader) stopped, the new leader moved node-2's
    /// untouched frontend to node-1. It had node-2's state report but not yet
    /// its readiness and capability reports, so node-2 looked unready.
    #[test]
    fn a_node_whose_readiness_has_not_arrived_keeps_its_replicas() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 3));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n3"]));
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2")]);
        let cache_with_n2_unready = || {
            let mut cache = ClusterStateCache::new();
            cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
            let mut n2 = sched_node("n2", 4000, BTreeMap::new());
            n2.ready = false;
            cache.set_node(n2);
            cache
        };

        let unheard = HashSet::from([NodeId::new("n2")]);
        let decisions = plan_scheduling_pass_with_dns(
            &mut cache_with_n2_unready(),
            &desired,
            &alive,
            &HashSet::new(),
            &mut QuotaLedger::default(),
            false,
            &unheard,
        );
        let nodes = nodes_of(&decisions[0]);
        assert_eq!(nodes[..2], ["n1", "n2"], "{nodes:?}");

        // Heard, and not ready: that's evidence, and the replica moves.
        let decisions = plan_scheduling_pass_with_dns(
            &mut cache_with_n2_unready(),
            &desired,
            &alive,
            &HashSet::new(),
            &mut QuotaLedger::default(),
            false,
            &HashSet::new(),
        );
        assert_eq!(nodes_of(&decisions[0]), ["n1", "n1", "n1"]);
    }

    #[test]
    fn only_fresh_nodes_missing_readiness_or_capability_evidence_are_unheard() {
        let alive: HashSet<NodeId> = [
            "complete",
            "no-readiness",
            "no-capability",
            "stale",
            "silent",
        ]
        .into_iter()
        .map(NodeId::new)
        .collect();
        let mut reports = AggregatedState::default();
        for name in ["complete", "no-readiness", "no-capability", "stale"] {
            reports.reports.insert(NodeId::new(name), report(4000, 0));
        }
        for name in ["complete", "no-capability"] {
            reports
                .readiness
                .insert(NodeId::new(name), readiness(name, true));
        }
        for name in ["complete", "no-readiness"] {
            reports.capabilities.insert(
                NodeId::new(name),
                crate::reporting::types::NodeCapabilityReport {
                    node_id: NodeId::new(name),
                    capabilities: Default::default(),
                    egress_enforcement: Vec::new(),
                    egress_degraded: false,
                    egress_affected_workloads: Vec::new(),
                },
            );
        }
        reports.stale_nodes.push(NodeId::new("stale"));

        let unheard = unheard_nodes(&alive, &reports);

        assert_eq!(
            unheard,
            HashSet::from([NodeId::new("no-readiness"), NodeId::new("no-capability")]),
            "a stale node lost its evidence; a silent one isn't in the cache at all"
        );
    }

    #[test]
    fn scaling_down_keeps_the_first_placements() {
        let app = AppId::new("frontend", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(100, 2));
        desired
            .scheduling
            .insert(app.clone(), placed_on(&["n1", "n2", "n3"]));
        let mut cache = ClusterStateCache::new();
        for name in ["n1", "n2", "n3"] {
            cache.set_node(sched_node(name, 4000, BTreeMap::new()));
        }
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2"), NodeId::new("n3")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(nodes_of(&decisions[0]), ["n1", "n2"]);
    }

    /// A cordoned (upgrade) node receives nothing.
    #[test]
    fn decommissioned_node_is_not_scheduled_from_stale_reports() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("old-worker", 4000, BTreeMap::new()));
        cache.set_node(sched_node("replacement", 4000, BTreeMap::new()));
        let mut desired = DesiredState::default();
        desired.security_state.crl.retired_nodes.insert(
            "old-worker".into(),
            crate::cluster::retirement::NodeRetirement {
                node_id: "old-worker".into(),
                retired_by: "operator".into(),
                reason: "powered off".into(),
                retired_at_unix_ms: 30,
                released_placements: Default::default(),
                released_registry_writers: Default::default(),
                released_node_fault: None,
                released_endpoint_consumer: false,
            },
        );
        let app = AppId::new("web", "default");
        let mut spec = app_spec(100, 1);
        spec.replicas = Replicas::DaemonSet;
        desired.apps.insert(app, spec);
        let alive = HashSet::from([NodeId::new("old-worker"), NodeId::new("replacement")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].placements.len(), 1);
        assert_eq!(
            decisions[0].placements[0].node_id,
            NodeId::new("replacement")
        );
    }

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

    /// M25b: a daemon set with an ineligible (not-ready) alive node converges
    /// once it's placed on every *eligible* node — it must not re-commit an
    /// identical decision every tick.
    #[test]
    fn daemon_converges_against_the_eligible_node_set_not_all_alive() {
        let mut desired = DesiredState::default();
        let a = AppId::new("mon", "system");
        let mut spec = app_spec(100, 1);
        spec.replicas = Replicas::DaemonSet;
        desired.apps.insert(a.clone(), spec);

        // Two nodes ready, one alive-but-not-ready (ineligible).
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        let mut not_ready = sched_node("n3", 4000, BTreeMap::new());
        not_ready.ready = false;
        cache.set_node(not_ready);
        let alive = HashSet::from([NodeId::new("n1"), NodeId::new("n2"), NodeId::new("n3")]);

        // First pass places on the two eligible nodes.
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(
            decisions[0].placements.len(),
            2,
            "placed on the eligible nodes"
        );
        desired
            .scheduling
            .insert(a.clone(), decisions[0].placements.clone());

        // Second pass: the committed placements already cover every eligible
        // node, so the daemon is converged and produces NO new decision — no
        // per-tick Raft churn.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("n1", 4000, BTreeMap::new()));
        cache.set_node(sched_node("n2", 4000, BTreeMap::new()));
        let mut not_ready = sched_node("n3", 4000, BTreeMap::new());
        not_ready.ready = false;
        cache.set_node(not_ready);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert!(
            decisions.is_empty(),
            "a daemon covering every eligible node must not re-plan: {decisions:?}"
        );
    }

    /// M26: the autoscaler metric match is namespace-qualified and exact — it
    /// no longer pools distinct apps by a bare-name substring.
    #[test]
    fn autoscale_metric_match_is_namespace_qualified_and_exact() {
        let web_prod = AppId::new("web", "prod");
        // Exact qualified match.
        assert!(aggregate_is_for_app(
            r#"{"app":"prod/web","pid":"12"}"#,
            &web_prod
        ));
        // A different namespace's same-named app must NOT match.
        assert!(!aggregate_is_for_app(r#"{"app":"staging/web"}"#, &web_prod));
        // A substring collision (webhook) must NOT match.
        assert!(!aggregate_is_for_app(
            r#"{"app":"prod/webhook"}"#,
            &web_prod
        ));
        // Missing/omitted app label, and non-JSON, don't match.
        assert!(!aggregate_is_for_app(r#"{"pid":"12"}"#, &web_prod));
        assert!(!aggregate_is_for_app("not json", &web_prod));
    }

    /// M25a: a partially-placed fixed-replica app that then fails must not leak
    /// its phantom reservations into the shared pass cache and starve a later
    /// app that would otherwise fit.
    #[test]
    fn failed_placement_does_not_leak_reservations_to_later_apps() {
        // One node with room for exactly one 600m replica.
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("solo", 1000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("solo")]);

        let mut desired = DesiredState::default();
        // App "a" wants 2 replicas of 600m — only one fits, so it fails after
        // reserving one node. It sorts before "b".
        let a = AppId::new("a", "prod");
        let b = AppId::new("b", "prod");
        desired.apps.insert(a, app_spec(600, 2));
        desired.apps.insert(b.clone(), app_spec(600, 1));

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        // "a" fails (only 1 of 2 fits). Its phantom reservation must be
        // discarded so "b" (a single 600m replica) still places on the node.
        assert!(
            decisions.iter().any(|d| d.app_id == b),
            "the later app must still place after a failed app's reservations are discarded: {decisions:?}"
        );
    }

    /// C12: a namespace quota must count already-converged apps, so a new app
    /// that pushes the namespace over budget is rejected. Before seeding the
    /// ledger from committed state, the converged app contributed nothing and
    /// the new app was admitted against a clean slate.
    #[test]
    fn converged_apps_count_against_the_namespace_quota() {
        let mut cache = ClusterStateCache::new();
        // Ample node capacity — the only limit under test is the quota.
        cache.set_node(sched_node("big", 100_000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("big")]);

        let mut desired = DesiredState::default();
        let a = AppId::new("a", "prod");
        let b = AppId::new("b", "prod");
        desired.apps.insert(a.clone(), app_spec(600, 1));
        desired.apps.insert(b.clone(), app_spec(600, 1));
        // App "a" is already converged on the live, ready node.
        desired.scheduling.insert(
            a.clone(),
            vec![crate::meat::types::Placement {
                node_id: NodeId::new("big"),
                resources: Resources::new(600, 0, 0),
                ordinal: 0,
            }],
        );

        // prod budget: 1000m — fits one 600m app, not two.
        let quota = NamespaceQuota {
            namespace: "prod".to_string(),
            max_cpu_millicores: Some(1000),
            max_memory_bytes: None,
            max_gpus: None,
            max_apps: None,
            max_replicas: None,
        };
        let mut quotas = QuotaLedger::new(HashMap::from([("prod".to_string(), quota)]));

        let decisions = plan_scheduling_pass(&mut cache, &desired, &alive, &mut quotas);

        // "a" is converged (no new decision); "b" is rejected because a's
        // 600m already fills the 1000m budget. Nothing new is scheduled.
        assert!(
            !decisions.iter().any(|d| d.app_id == b),
            "the new app must be rejected once the converged app is counted: {decisions:?}"
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
                ordinal: 0,
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

    #[test]
    fn egress_placement_is_replanned_when_node_loses_enforcement() {
        let mut desired = DesiredState::default();
        let app = AppId::new("payer", "prod");
        let mut spec = app_spec(100, 1);
        spec.egress = Some(crate::config::app::EgressSpec {
            allow: vec!["192.0.2.10:443".to_string()],
            allow_franchise: Vec::new(),
        });
        desired.apps.insert(app.clone(), spec);
        desired.scheduling.insert(
            app.clone(),
            vec![crate::meat::types::Placement {
                node_id: NodeId::new("lost-hooks"),
                resources: Resources::new(100, 0, 0),
                ordinal: 0,
            }],
        );

        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("lost-hooks", 4000, BTreeMap::new()));
        let mut capable = sched_node("guarded", 4000, BTreeMap::new());
        capable.capabilities.egress = crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        };
        cache.set_node(capable);
        let alive = HashSet::from([NodeId::new("lost-hooks"), NodeId::new("guarded")]);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert_eq!(decisions.len(), 1, "capability loss must force a re-plan");
        assert_eq!(decisions[0].placements[0].node_id, NodeId::new("guarded"));
    }

    #[test]
    fn dns_placement_is_replanned_when_node_loses_resolver_capability() {
        let mut desired = DesiredState::default();
        let app = AppId::new("client", "prod");
        desired.apps.insert(app.clone(), app_spec(100, 1));
        desired.scheduling.insert(
            app.clone(),
            vec![crate::meat::types::Placement {
                node_id: NodeId::new("dns-lost"),
                resources: Resources::new(100, 0, 0),
                ordinal: 0,
            }],
        );

        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("dns-lost", 4000, BTreeMap::new()));
        let mut capable = sched_node("dns-ready", 4000, BTreeMap::new());
        capable.capabilities.dns = crate::onion::dns::DnsCapability {
            enabled: true,
            ready: true,
            ipv4: true,
            ipv6: false,
            workload_reachable: true,
        };
        cache.set_node(capable);
        let alive = HashSet::from([NodeId::new("dns-lost"), NodeId::new("dns-ready")]);

        let decisions = plan_scheduling_pass_with_dns(
            &mut cache,
            &desired,
            &alive,
            &HashSet::new(),
            &mut QuotaLedger::default(),
            true,
            &HashSet::new(),
        );

        assert_eq!(decisions.len(), 1, "DNS capability loss must re-plan");
        assert_eq!(decisions[0].placements[0].node_id, NodeId::new("dns-ready"));
    }

    #[test]
    fn dns_requirement_stays_fail_closed_when_all_capability_leases_are_absent() {
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(AppId::new("client", "prod"), app_spec(100, 1));

        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node(
            "reported-but-no-dns-lease",
            4000,
            BTreeMap::new(),
        ));
        let alive = HashSet::from([NodeId::new("reported-but-no-dns-lease")]);

        let decisions = plan_scheduling_pass_with_dns(
            &mut cache,
            &desired,
            &alive,
            &HashSet::new(),
            &mut QuotaLedger::default(),
            true,
            &HashSet::new(),
        );
        assert!(
            decisions.is_empty(),
            "losing every DNS lease must not be interpreted as DNS being disabled"
        );
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

    /// An over-quota app passes validation and `relish apply` writes it to
    /// desired state, but the leader's scheduling pass never places it. The
    /// pass says why, so the leader can record the reason in council state
    /// (#326) instead of leaving only a line in its log.
    #[test]
    fn over_quota_app_is_not_placed_and_the_pass_says_why() {
        let config = crate::config::Config::parse(
            r#"
            [namespace.prod]
            cpu = "1000m"

            [app.greedy]
            image = "x:1"
            namespace = "prod"
            replicas = 2
            cpu = "800m"
        "#,
        )
        .unwrap();
        config
            .validate_against(&[])
            .expect("apply-time validation doesn't check the budget");

        let mut desired = DesiredState::default();
        for write in crate::council::apply::config_to_desired_writes(&config) {
            match write {
                crate::council::types::RaftRequest::NamespaceSpec { name, spec } => {
                    desired.namespaces.insert(name, *spec);
                }
                crate::council::types::RaftRequest::AppSpec { app_id, spec } => {
                    desired.apps.insert(app_id, *spec);
                }
                other => panic!("unexpected write for this config: {other:?}"),
            }
        }
        let greedy = AppId::new("greedy", "prod");
        assert!(
            desired.apps.contains_key(&greedy),
            "apply writes the over-quota app to desired state"
        );

        let plan = quota_pass(&desired);
        assert!(
            plan.decisions.is_empty(),
            "the scheduling pass leaves the over-quota app unplaced: {:?}",
            plan.decisions
        );
        assert_eq!(
            plan.quota_blocked.get(&greedy),
            Some(&crate::meat::quota::QuotaError::CpuExceeded {
                namespace: "prod".to_string(),
                current: 0,
                requested: 1600,
                limit: 1000,
            }),
            "the pass records why the app isn't placed"
        );
    }

    /// Plan one pass on a roomy node with the quotas `desired` declares.
    fn quota_pass(desired: &DesiredState) -> PassPlan {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("big", 10000, BTreeMap::new()));
        let mut quotas = crate::meat::quota::ledger_from_namespaces(&desired.namespaces);
        let alive = HashSet::from([NodeId::new("big")]);
        plan_pass(
            &mut cache,
            desired,
            &alive,
            &HashSet::new(),
            &mut quotas,
            false,
            &HashSet::new(),
        )
    }

    fn cpu_namespace(cpu: &str) -> crate::config::NamespaceSpec {
        crate::config::NamespaceSpec {
            cpu: Some(cpu.to_string()),
            memory: None,
            gpu: None,
            max_apps: None,
            max_replicas: None,
        }
    }

    /// Growing the namespace's budget clears the reason on the next pass,
    /// and the app is placed.
    #[test]
    fn quota_block_clears_once_the_namespace_quota_grows() {
        let mut desired = DesiredState::default();
        desired
            .namespaces
            .insert("prod".to_string(), cpu_namespace("1000m"));
        let greedy = AppId::new("greedy", "prod");
        desired.apps.insert(greedy.clone(), app_spec(800, 2));
        assert!(quota_pass(&desired).quota_blocked.contains_key(&greedy));

        desired
            .namespaces
            .insert("prod".to_string(), cpu_namespace("2000m"));
        let plan = quota_pass(&desired);
        assert!(plan.quota_blocked.is_empty(), "{:?}", plan.quota_blocked);
        assert_eq!(plan.decisions.len(), 1, "the app now fits and is placed");
    }

    /// Removing the apps that used the budget clears the reason too.
    #[test]
    fn quota_block_clears_once_other_apps_leave_the_namespace() {
        let mut desired = DesiredState::default();
        desired
            .namespaces
            .insert("prod".to_string(), cpu_namespace("1000m"));
        let first = AppId::new("a-first", "prod");
        let second = AppId::new("b-second", "prod");
        desired.apps.insert(first.clone(), app_spec(600, 1));
        desired.apps.insert(second.clone(), app_spec(600, 1));

        let plan = quota_pass(&desired);
        assert!(!plan.quota_blocked.contains_key(&first));
        assert!(
            matches!(
                plan.quota_blocked.get(&second),
                Some(crate::meat::quota::QuotaError::CpuExceeded { current: 600, .. })
            ),
            "the second app is blocked by the first one's usage: {:?}",
            plan.quota_blocked
        );

        desired.apps.remove(&first);
        let plan = quota_pass(&desired);
        assert!(plan.quota_blocked.is_empty(), "{:?}", plan.quota_blocked);
        assert_eq!(plan.decisions.len(), 1);
    }

    /// The leader proposes a `QuotaBlocked` write only when the set of
    /// blocked apps or a reason changes. Every pass recomputes it, so an
    /// unconditional write would churn the Raft log once a tick.
    #[test]
    fn an_unchanged_quota_block_writes_nothing() {
        let greedy = AppId::new("greedy", "prod");
        let reason = |current| crate::meat::quota::QuotaError::CpuExceeded {
            namespace: "prod".to_string(),
            current,
            requested: 1600,
            limit: 1000,
        };
        let recorded = HashMap::from([(greedy.clone(), reason(0))]);

        let same = BTreeMap::from([(greedy.clone(), reason(0))]);
        assert_eq!(quota_blocked_update(&recorded, &same), None);

        let moved = BTreeMap::from([(greedy.clone(), reason(200))]);
        assert_eq!(
            quota_blocked_update(&recorded, &moved),
            Some(RaftRequest::QuotaBlocked {
                blocked: vec![(greedy.clone(), reason(200))],
            }),
            "a changed reason is written"
        );

        assert_eq!(
            quota_blocked_update(&recorded, &BTreeMap::new()),
            Some(RaftRequest::QuotaBlocked { blocked: vec![] }),
            "an app that fits again is cleared"
        );
        assert_eq!(
            quota_blocked_update(&HashMap::new(), &BTreeMap::new()),
            None,
            "nothing blocked and nothing recorded writes nothing"
        );
    }

    /// Run several passes over the same over-quota state, applying what the
    /// leader would write after each: the first records the block, and the
    /// rest find nothing to write.
    #[test]
    fn a_steady_over_quota_cluster_stops_writing_after_the_first_pass() {
        let mut desired = DesiredState::default();
        desired
            .namespaces
            .insert("prod".to_string(), cpu_namespace("1000m"));
        desired
            .apps
            .insert(AppId::new("greedy", "prod"), app_spec(800, 2));

        let first = quota_pass(&desired);
        let Some(RaftRequest::QuotaBlocked { blocked }) =
            quota_blocked_update(&desired.quota_blocked, &first.quota_blocked)
        else {
            panic!("the first pass records the block");
        };
        desired.quota_blocked = blocked.into_iter().collect();

        for _ in 0..3 {
            let again = quota_pass(&desired);
            assert_eq!(
                quota_blocked_update(&desired.quota_blocked, &again.quota_blocked),
                None
            );
        }
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

#[cfg(test)]
mod audit_stale_endpoints {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::AppId;
    use crate::reporting::types::*;
    use std::time::{Instant, SystemTime};
    fn app_spec(cpu_request: u64, replicas: u32) -> AppSpec {
        let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
        spec.replicas = Replicas::Fixed(replicas);
        spec.cpu = Some(crate::config::types::ResourceRange {
            request: cpu_request,
            limit: cpu_request,
        });
        spec
    }
    #[test]
    fn stale_reports_must_not_publish_healthy_endpoints() {
        let app = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        let mut spec = app_spec(100, 1);
        spec.port = Some(8080);
        desired.apps.insert(app, spec);
        let id = NodeId::new("n1");
        let members = vec![MembershipSnapshot {
            node_id: id.clone(),
            address: "10.1.1.1:9116".parse().unwrap(),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: Default::default(),
            first_seen: Instant::now(),
            resources: None,
        }];
        let mut reports = AggregatedState::default();
        reports.stale_nodes.push(id.clone());
        reports.reports.insert(
            id.clone(),
            StateReport {
                has_buildah: false,
                node_id: id,
                timestamp: SystemTime::now(),
                cached_specs: vec![],
                resource_usage: ResourceUsage::default(),
                event_log: vec![],
                running_apps: vec![RunningApp {
                    execution: None,
                    app_name: "web".into(),
                    namespace: "default".into(),
                    instance_id: 0,
                    image: "x:1".into(),
                    port: Some(30000),
                    health_status: ReportHealthStatus::Healthy,
                    uptime: Duration::ZERO,
                    resource_usage: AppResourceUsage::default(),
                }],
            },
        );
        let backends = |catalog: &crate::onion::catalog::EndpointCatalog| {
            catalog
                .services
                .values()
                .flat_map(|s| s.backends.clone())
                .collect::<Vec<_>>()
        };

        // A stale report publishes nothing, healthy or not.
        let catalog = build_endpoint_catalog(&members, &reports, &desired).unwrap();
        assert!(
            backends(&catalog).is_empty(),
            "a stale report must not publish its last backends"
        );

        // The committed catalogue still holds a healthy backend from when the
        // report was fresh.
        let mut fresh = reports.clone();
        fresh.stale_nodes.clear();
        desired.endpoint_catalog = build_endpoint_catalog(&members, &fresh, &desired).unwrap();
        assert_eq!(backends(&desired.endpoint_catalog).len(), 1);

        // Its payload is evicted but the node stays stale: withdrawn, not carried.
        let mut evicted = AggregatedState::default();
        evicted.stale_nodes.push(NodeId::new("n1"));
        let catalog = build_endpoint_catalog(&members, &evicted, &desired).unwrap();
        assert!(
            backends(&catalog).is_empty(),
            "report eviction must not resurrect an expired backend"
        );

        // Within a new leader's first window (no report, not yet stale) the
        // committed backend is kept.
        let catalog =
            build_endpoint_catalog(&members, &AggregatedState::default(), &desired).unwrap();
        assert_eq!(backends(&catalog).len(), 1);
    }

    #[test]
    fn only_a_stale_planned_member_blocks_capacity_admission() {
        let member = |name: &str| MembershipSnapshot {
            node_id: NodeId::new(name),
            address: "10.1.1.1:9116".parse().unwrap(),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: Default::default(),
            first_seen: Instant::now(),
            resources: None,
        };
        let members = vec![member("planned")];
        let mut reports = AggregatedState::default();
        assert!(!has_stale_member(&members, &reports));
        reports.stale_nodes.push(NodeId::new("retired"));
        assert!(!has_stale_member(&members, &reports));
        reports.stale_nodes.push(NodeId::new("planned"));
        assert!(has_stale_member(&members, &reports));
    }
}

#[cfg(test)]
mod audit_pending_reservations {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::quota::QuotaLedger;
    use crate::meat::{cluster_state::SchedulerNodeState, types::AppId};
    fn sched_node(name: &str, cpu: u64, labels: BTreeMap<String, String>) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
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
    #[test]
    fn pending_placements_must_reserve_capacity_across_ticks() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("solo", 1000, BTreeMap::new()));
        let old_report = cache.clone();
        let alive = HashSet::from([NodeId::new("solo")]);
        let a = AppId::new("a", "default");
        let b = AppId::new("b", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(a.clone(), app_spec(600, 1));
        desired.apps.insert(b.clone(), app_spec(600, 1));
        let first = plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(first.len(), 1);
        for decision in first {
            desired
                .scheduling
                .insert(decision.app_id, decision.placements);
        }
        let second = plan_scheduling_pass(
            &mut old_report.clone(),
            &desired,
            &alive,
            &mut QuotaLedger::default(),
        );
        eprintln!("second pass with same report: {second:?}");
        assert!(
            second.iter().all(|d| d.app_id != b),
            "second tick overbooks the same 1000m node with two 600m apps"
        );
    }
}

#[cfg(test)]
mod audit_daemon_self_reservation {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::quota::QuotaLedger;
    use crate::meat::{cluster_state::SchedulerNodeState, types::AppId};
    fn sched_node(name: &str, cpu: u64, labels: BTreeMap<String, String>) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
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
    #[test]
    fn daemon_placement_must_not_evict_a_running_instance_for_its_own_reservation() {
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node("a", 1000, BTreeMap::new()));
        cache.set_node(sched_node("b", 1000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("a"), NodeId::new("b")]);
        let app = AppId::new("daemon", "default");
        let mut spec = app_spec(600, 1);
        spec.replicas = Replicas::DaemonSet;
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), spec);
        let first = plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert_eq!(first[0].placements.len(), 2);
        desired
            .scheduling
            .insert(app.clone(), first[0].placements.clone());
        // Node a has reported its started daemon; node b has not started/reported yet.
        let mut a = sched_node("a", 1000, BTreeMap::new());
        a.allocated = Resources::new(600, 0, 0);
        a.app_replicas.insert(app.clone(), 1);
        let mut fresh = ClusterStateCache::new();
        fresh.set_node(a);
        fresh.set_node(sched_node("b", 1000, BTreeMap::new()));
        let second =
            plan_scheduling_pass(&mut fresh, &desired, &alive, &mut QuotaLedger::default());
        eprintln!("daemon second pass: {second:?}");
        assert!(
            second.is_empty()
                || second[0]
                    .placements
                    .iter()
                    .any(|p| p.node_id == NodeId::new("a")),
            "running daemon was removed because it cannot fit a second copy"
        );
    }
}

/// Where #432's reconstructed reservations meet #433's daemon credit: a
/// daemon whose committed copies haven't been reported yet keeps every
/// placement, and its reconstructed footprint still blocks other apps.
#[cfg(test)]
mod reconstructed_daemon_reservations {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::quota::QuotaLedger;
    use crate::meat::{cluster_state::SchedulerNodeState, types::AppId};

    fn empty_node(name: &str) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(1000, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels: BTreeMap::new(),
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
            uptime_secs: 86400,
            cached_images: Default::default(),
        }
    }

    fn requesting(cpu: u64, replicas: Replicas) -> AppSpec {
        let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
        spec.replicas = replicas;
        spec.cpu = Some(crate::config::types::ResourceRange {
            request: cpu,
            limit: cpu,
        });
        spec
    }

    fn lagging_cache() -> ClusterStateCache {
        let mut cache = ClusterStateCache::new();
        cache.set_node(empty_node("a"));
        cache.set_node(empty_node("b"));
        cache
    }

    #[test]
    fn an_unreported_daemon_keeps_its_placements_and_its_room() {
        let alive = HashSet::from([NodeId::new("a"), NodeId::new("b")]);
        let daemon = AppId::new("agent", "default");
        let web = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(daemon.clone(), requesting(600, Replicas::DaemonSet));
        let first = plan_scheduling_pass(
            &mut lagging_cache(),
            &desired,
            &alive,
            &mut QuotaLedger::default(),
        );
        assert_eq!(first[0].placements.len(), 2);
        desired
            .scheduling
            .insert(daemon.clone(), first[0].placements.clone());
        desired
            .apps
            .insert(web.clone(), requesting(600, Replicas::Fixed(1)));

        // Neither node has reported the daemon yet.
        let mut cache = lagging_cache();
        let second =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());

        assert!(
            second.iter().all(|d| d.app_id != daemon),
            "the daemon was re-planned: {second:?}"
        );
        assert!(
            second.iter().all(|d| d.app_id != web),
            "web was placed into room the daemon holds: {second:?}"
        );
        for node in ["a", "b"] {
            let node = cache.get_node(&NodeId::new(node)).unwrap();
            assert_eq!(node.allocated.cpu_millicores, 600);
            assert_eq!(node.replicas_of(&daemon), 1);
        }
    }
}

#[cfg(test)]
mod audit_placement_revalidation {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::quota::QuotaLedger;
    use crate::meat::{
        cluster_state::SchedulerNodeState,
        types::{AppId, Placement},
    };
    fn sched_node(name: &str, cpu: u64, labels: BTreeMap<String, String>) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels,
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
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
    #[test]
    fn resource_increase_must_be_readmitted() {
        let app = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), app_spec(2000, 1));
        desired.scheduling.insert(
            app.clone(),
            vec![Placement {
                node_id: NodeId::new("small"),
                resources: Resources::new(600, 0, 0),
                ordinal: 0,
            }],
        );
        let mut cache = ClusterStateCache::new();
        let mut small = sched_node("small", 1000, BTreeMap::new());
        small.allocated = Resources::new(600, 0, 0);
        cache.set_node(small);
        cache.set_node(sched_node("large", 4000, BTreeMap::new()));
        let alive = HashSet::from([NodeId::new("small"), NodeId::new("large")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        eprintln!("updated spec decisions: {decisions:?}");
        assert!(
            !decisions.is_empty(),
            "2000m app remains converged on a 1000m node with old 600m placement"
        );
    }

    #[test]
    fn changed_required_labels_must_move_a_placement() {
        let app = AppId::new("web", "default");
        let spec: AppSpec =
            toml::from_str("image='x:1'\n[placement]\nrequired=['zone=west']").unwrap();
        let mut desired = DesiredState::default();
        desired.apps.insert(app.clone(), spec);
        desired.scheduling.insert(
            app.clone(),
            vec![Placement {
                node_id: NodeId::new("east"),
                resources: Resources::default(),
                ordinal: 0,
            }],
        );
        let mut cache = ClusterStateCache::new();
        cache.set_node(sched_node(
            "east",
            1000,
            BTreeMap::from([("zone".into(), "east".into())]),
        ));
        cache.set_node(sched_node(
            "west",
            1000,
            BTreeMap::from([("zone".into(), "west".into())]),
        ));
        let alive = HashSet::from([NodeId::new("east"), NodeId::new("west")]);
        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
        assert!(
            !decisions.is_empty(),
            "a new hard placement constraint is ignored"
        );
    }
    #[test]
    fn audit_changed_daemon_request_keeps_old_running_capacity_reserved() {
        let app = AppId::new("a-daemon", "default");
        let other = AppId::new("z-other", "default");
        let mut desired = DesiredState::default();
        let mut spec = app_spec(2000, 1);
        spec.replicas = Replicas::DaemonSet;
        desired.apps.insert(app.clone(), spec);
        desired.apps.insert(other.clone(), app_spec(1000, 1));
        // AppSpec removed the old assignment, but its 600m execution has not
        // stopped yet. It cannot pay for a newly admitted 2000m assignment.
        let mut node = sched_node("solo", 3000, BTreeMap::new());
        node.allocated = Resources::new(600, 0, 0);
        node.app_replicas.insert(app.clone(), 1);
        let mut cache = ClusterStateCache::new();
        cache.set_node(node);
        let decisions = plan_scheduling_pass(
            &mut cache,
            &desired,
            &HashSet::from([NodeId::new("solo")]),
            &mut QuotaLedger::default(),
        );
        assert!(decisions.iter().any(|d| d.app_id == app));
        assert!(!decisions.iter().any(|d| d.app_id == other));
        assert_eq!(
            cache
                .get_node(&NodeId::new("solo"))
                .unwrap()
                .allocated
                .cpu_millicores,
            2600
        );
    }
}

/// #434 meets #423 and #433: a changed spec is revalidated where it runs.
/// A replica stays on its node when the node still admits it, so a resource
/// change rolls in place instead of retiring every replica at once; a
/// managed-volume app never leaves its home over resources; and only a
/// changed hard selector moves it.
#[cfg(test)]
mod revalidation_in_place {
    use super::*;
    use crate::config::Replicas;
    use crate::council::types::DesiredState;
    use crate::meat::quota::QuotaLedger;
    use crate::meat::{
        cluster_state::SchedulerNodeState,
        types::{AppId, Placement},
    };

    fn node(name: &str, cpu: u64, zone: &str) -> SchedulerNodeState {
        SchedulerNodeState {
            node_id: NodeId::new(name),
            allocatable: Resources::new(cpu, 8 * 1024 * 1024 * 1024, 0),
            allocated: Resources::default(),
            labels: BTreeMap::from([("zone".to_string(), zone.to_string())]),
            ready: true,
            capabilities: Default::default(),
            app_replicas: Default::default(),
            uptime_secs: 86400,
            cached_images: Default::default(),
        }
    }

    fn requesting(cpu: u64, replicas: Replicas) -> AppSpec {
        let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
        spec.replicas = replicas;
        spec.cpu = Some(crate::config::types::ResourceRange {
            request: cpu,
            limit: cpu,
        });
        spec
    }

    fn with_volume(mut spec: AppSpec) -> AppSpec {
        spec.volumes.push(crate::config::types::VolumeSpec {
            path: "/data".into(),
            source: None,
            size: None,
        });
        spec
    }

    fn requiring(mut spec: AppSpec, zone: &str) -> AppSpec {
        let with_selector: AppSpec = toml::from_str(&format!(
            "image = 'x:1'\n[placement]\nrequired = ['zone={zone}']"
        ))
        .unwrap();
        spec.placement = with_selector.placement;
        spec
    }

    /// `home` runs one 600m replica of `app`; `busy` is fuller, so the
    /// bin-packing scheduler would pick it for anything placed afresh.
    fn running_on_home(app: &AppId, home_cpu: u64) -> ClusterStateCache {
        let mut home = node("home", home_cpu, "east");
        home.allocated = Resources::new(600, 0, 0);
        home.app_replicas.insert(app.clone(), 1);
        let mut busy = node("busy", 4000, "west");
        busy.allocated = Resources::new(2000, 0, 0);
        let mut cache = ClusterStateCache::new();
        cache.set_node(home);
        cache.set_node(busy);
        cache
    }

    fn committed_on_home(desired: &mut DesiredState, app: &AppId) {
        desired.scheduling.insert(
            app.clone(),
            vec![Placement {
                node_id: NodeId::new("home"),
                resources: Resources::new(600, 0, 0),
                ordinal: 0,
            }],
        );
        desired
            .last_placed_nodes
            .insert(app.clone(), vec![NodeId::new("home")]);
    }

    fn alive() -> HashSet<NodeId> {
        HashSet::from([NodeId::new("home"), NodeId::new("busy")])
    }

    fn placements_of(
        decisions: &[crate::meat::types::SchedulingDecision],
        app: &AppId,
    ) -> Vec<(String, u64)> {
        decisions
            .iter()
            .find(|d| &d.app_id == app)
            .map(|d| {
                d.placements
                    .iter()
                    .map(|p| (p.node_id.0.clone(), p.resources.cpu_millicores))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ordinary_apps_respect_unreported_batch_reservations_and_exact_namespace_dedup() {
        for reported_namespace in [None, Some("default"), Some("other")] {
            let mut desired = DesiredState::default();
            let app = AppId::new("web", "default");
            desired
                .apps
                .insert(app.clone(), requesting(4000, Replicas::Fixed(1)));
            let batch: crate::meat::batch_tracker::BatchRecord = serde_json::from_value(serde_json::json!({
                "submitted_at_epoch_secs": 1,
                "jobs": [{"name":"logical", "namespace":"default", "execution_name":"opaque-owned",
                    "spec_digest":"a".repeat(64), "node":"home", "status":"Pending",
                    "resources":{"cpu_millicores":5000,"memory_bytes":0,"gpus":0}}]
            })).unwrap();
            desired.batch_state.register(batch).unwrap();
            let mut home = node("home", 8000, "east");
            if let Some(namespace) = reported_namespace {
                home.allocated = Resources::new(5000, 0, 0);
                home.app_replicas
                    .insert(AppId::new("opaque-owned", namespace), 1);
            }
            let mut cache = ClusterStateCache::new();
            cache.set_node(home);
            let decisions = plan_scheduling_pass(
                &mut cache,
                &desired,
                &HashSet::from([NodeId::new("home")]),
                &mut QuotaLedger::default(),
            );
            assert!(
                placements_of(&decisions, &app).is_empty(),
                "namespace={reported_namespace:?}: {decisions:?}"
            );
            let allocated = cache
                .get_node(&NodeId::new("home"))
                .unwrap()
                .allocated
                .cpu_millicores;
            let expected = if reported_namespace == Some("other") {
                10000
            } else {
                5000
            };
            assert_eq!(allocated, expected, "namespace={reported_namespace:?}");
        }
    }

    #[test]
    fn a_resource_change_that_still_fits_keeps_the_replica_on_its_node() {
        let app = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(app.clone(), requesting(900, Replicas::Fixed(1)));
        committed_on_home(&mut desired, &app);
        let mut cache = running_on_home(&app, 1000);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive(), &mut QuotaLedger::default());

        assert_eq!(placements_of(&decisions, &app), [("home".to_string(), 900)]);
        let home = cache.get_node(&NodeId::new("home")).unwrap();
        assert_eq!(home.allocated.cpu_millicores, 900);
        assert_eq!(home.replicas_of(&app), 1);
    }

    #[test]
    fn a_resource_increase_that_no_longer_fits_moves_a_stateless_replica() {
        let app = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(app.clone(), requesting(1500, Replicas::Fixed(1)));
        committed_on_home(&mut desired, &app);
        let mut cache = running_on_home(&app, 1000);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive(), &mut QuotaLedger::default());

        assert_eq!(
            placements_of(&decisions, &app),
            [("busy".to_string(), 1500)]
        );
    }

    #[test]
    fn a_volume_apps_resource_change_never_moves_it_off_its_home() {
        let app = AppId::new("db", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(
            app.clone(),
            with_volume(requesting(1500, Replicas::Fixed(1))),
        );
        committed_on_home(&mut desired, &app);

        // Too big for home, and home restarting: neither moves it.
        for alive in [alive(), HashSet::from([NodeId::new("busy")])] {
            let mut cache = running_on_home(&app, 1000);
            let decisions =
                plan_scheduling_pass(&mut cache, &desired, &alive, &mut QuotaLedger::default());
            assert_eq!(
                placements_of(&decisions, &app),
                [("home".to_string(), 1500)]
            );
        }
    }

    #[test]
    fn a_changed_hard_selector_moves_a_running_volume_app() {
        let app = AppId::new("db", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(
            app.clone(),
            requiring(with_volume(requesting(600, Replicas::Fixed(1))), "west"),
        );
        committed_on_home(&mut desired, &app);
        let mut cache = running_on_home(&app, 1000);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive(), &mut QuotaLedger::default());

        assert_eq!(placements_of(&decisions, &app), [("busy".to_string(), 600)]);
    }

    #[test]
    fn a_hard_selector_the_node_still_matches_moves_nothing() {
        let app = AppId::new("db", "default");
        let mut desired = DesiredState::default();
        desired.apps.insert(
            app.clone(),
            requiring(with_volume(requesting(600, Replicas::Fixed(1))), "east"),
        );
        committed_on_home(&mut desired, &app);
        let mut cache = running_on_home(&app, 1000);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive(), &mut QuotaLedger::default());

        assert!(decisions.is_empty(), "{decisions:?}");
    }

    #[test]
    fn a_daemon_resource_change_credits_the_copy_it_replaces() {
        let app = AppId::new("agent", "default");
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(app.clone(), requesting(900, Replicas::DaemonSet));
        committed_on_home(&mut desired, &app);
        let mut cache = ClusterStateCache::new();
        let mut home = node("home", 1000, "east");
        home.allocated = Resources::new(600, 0, 0);
        home.app_replicas.insert(app.clone(), 1);
        cache.set_node(home);

        let decisions = plan_scheduling_pass(
            &mut cache,
            &desired,
            &HashSet::from([NodeId::new("home")]),
            &mut QuotaLedger::default(),
        );

        assert_eq!(placements_of(&decisions, &app), [("home".to_string(), 900)]);
        let home = cache.get_node(&NodeId::new("home")).unwrap();
        assert_eq!(home.allocated.cpu_millicores, 900);
    }

    /// #432's reconstruction meets the revalidation: home hasn't reported
    /// the replica yet, so its old footprint is reconstructed, credited and
    /// replaced by the new one, not stacked on top of it.
    #[test]
    fn an_unreported_replica_is_revalidated_against_its_reconstructed_footprint() {
        let app = AppId::new("web", "default");
        let mut desired = DesiredState::default();
        desired
            .apps
            .insert(app.clone(), requesting(900, Replicas::Fixed(1)));
        committed_on_home(&mut desired, &app);
        let mut cache = ClusterStateCache::new();
        cache.set_node(node("home", 1000, "east"));
        let mut busy = node("busy", 4000, "west");
        busy.allocated = Resources::new(2000, 0, 0);
        cache.set_node(busy);

        let decisions =
            plan_scheduling_pass(&mut cache, &desired, &alive(), &mut QuotaLedger::default());

        assert_eq!(placements_of(&decisions, &app), [("home".to_string(), 900)]);
        let home = cache.get_node(&NodeId::new("home")).unwrap();
        assert_eq!(home.allocated.cpu_millicores, 900);
    }
}

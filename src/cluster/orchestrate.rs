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
    /// Number of replicas of this app assigned to the node.
    pub replicas: u32,
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
            let decisions = plan_scheduling_pass_with_dns(
                &mut cache,
                &desired,
                &alive,
                &mut quotas,
                dns_required,
                &unheard,
            );

            if let Some(request) = capacity_request {
                use super::capacity::CapacityAdmissionError;
                use crate::meat::scheduler::{ScheduleError, Scheduler};

                let ready = reports.stale_nodes.is_empty()
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
    plan_scheduling_pass_with_dns(cache, desired, alive, quotas, false, &HashSet::new())
}

/// Plan a pass with the cluster's configured DNS requirement.
///
/// The requirement is an explicit configuration input. Deriving it from live
/// capability reports creates a fail-open edge: if every DNS lease expires,
/// absence would look exactly like an intentionally disabled resolver.
fn plan_scheduling_pass_with_dns(
    cache: &mut ClusterStateCache,
    desired: &crate::council::types::DesiredState,
    alive: &HashSet<NodeId>,
    quotas: &mut crate::meat::quota::QuotaLedger,
    dns_required: bool,
    unheard: &HashSet<NodeId>,
) -> Vec<crate::meat::types::SchedulingDecision> {
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
    let mut decisions = Vec::new();
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
            daemon_eligible_count(cache, spec, dns_required)
        } else {
            effective_replicas(spec, override_replicas, alive.len())
        };
        let converged = desired
            .scheduling
            .get(app_id)
            .map(|placements| {
                placements.len() == want
                    && placements
                        .iter()
                        .all(|p| placement_holds(p, spec, cache, alive, unheard, dns_required))
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
        // committed usage). A rejection is a deploy-time error surfaced
        // through the log, not a silent skip that leaves the app forever
        // pending without explanation.
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
        // A fixed-size app keeps the placements that still hold and only
        // places the rest. Losing one node of three must not move the
        // replicas on the other two: they're serving, and replacing them
        // would restart them for nothing.
        let kept: Vec<crate::meat::types::Placement> = match effective_spec.replicas {
            Replicas::Fixed(_) => desired
                .scheduling
                .get(app_id)
                .map(|placements| {
                    placements
                        .iter()
                        .filter(|p| placement_holds(p, spec, cache, alive, unheard, dns_required))
                        .take(want)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            Replicas::DaemonSet => Vec::new(),
        };
        if !kept.is_empty() {
            let missing = want - kept.len();
            if missing == 0 {
                decisions.push(crate::meat::types::SchedulingDecision {
                    app_id: app_id.clone(),
                    placements: kept,
                });
                continue;
            }
            effective_spec.replicas = Replicas::Fixed(missing as u32);
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
        let mut scheduler = Scheduler::new(std::mem::take(cache)).with_dns_required(dns_required);
        let result = scheduler.schedule_app(app_id, &effective_spec);
        match result {
            Ok(mut decision) => {
                *cache = scheduler.cluster;
                if !kept.is_empty() {
                    let added = std::mem::take(&mut decision.placements);
                    decision.placements = kept.into_iter().chain(added).collect();
                }
                decisions.push(decision);
            }
            Err(e) => {
                *cache = snapshot;
                eprintln!("scheduler: cannot place {app_id}: {e}");
            }
        }
    }
    decisions
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
/// ready or not capable, does lose them.
fn placement_holds(
    placement: &crate::meat::types::Placement,
    spec: &AppSpec,
    cache: &ClusterStateCache,
    alive: &HashSet<NodeId>,
    unheard: &HashSet<NodeId>,
    dns_required: bool,
) -> bool {
    if !alive.contains(&placement.node_id) {
        return false;
    }
    if unheard.contains(&placement.node_id) {
        return true;
    }
    let Some(node) = cache.get_node(&placement.node_id) else {
        return true;
    };
    let requires_egress = spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty());
    node.ready
        && (!requires_egress || node.capabilities.egress.can_enforce_allowlist())
        && (!dns_required || node.capabilities.dns.can_resolve_internal())
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
fn daemon_eligible_count(cache: &ClusterStateCache, spec: &AppSpec, dns_required: bool) -> usize {
    let resources = scheduler_resources(spec);
    let required = spec
        .placement
        .as_ref()
        .map(|p| crate::meat::scheduler::parse_label_list(&p.required))
        .unwrap_or_default();
    let requires_egress = spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty());
    crate::meat::filter::filter_nodes(&resources, &required, requires_egress, dns_required, cache)
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
            ready: reports
                .readiness
                .get(&member.node_id)
                .is_some_and(|report| report.evidence.ready)
                && capability_report.is_none_or(|capability| !capability.egress_degraded),
            capabilities: capability_report
                .map(|capability| capability.capabilities)
                .unwrap_or_default(),
            running_apps,
            uptime_secs: member.first_seen.elapsed().as_secs(),
            // Nothing reports cached images yet; locality scoring is
            // inert rather than fed guesses.
            cached_images: HashSet::new(),
        });
    }
    cache
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
) -> Option<(String, NodeAssignments)> {
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
    Some((leader_url, assignments))
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

            let Some((leader_url, assignments)) = poll_consumer(
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

            let mut seen: HashSet<(String, String)> = HashSet::new();
            for assignment in &assignments.apps {
                let key = (assignment.name.clone(), assignment.namespace.clone());
                seen.insert(key.clone());

                let mut spec = assignment.spec.clone();
                // The local agent runs exactly this node's share.
                spec.replicas = Replicas::Fixed(assignment.replicas);
                let fingerprint = serde_json::to_string(&spec).unwrap_or_default();
                if matches!(applied.get(&key), Some(AssignmentState::Applied { fingerprint: previous }) if previous == &fingerprint)
                {
                    continue; // already converged; don't redeploy
                }
                if !backoff.may_attempt(&key, &fingerprint, std::time::Instant::now()) {
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
                let terminal = deploy_outcome(event_rx, DEPLOY_TERMINAL_TIMEOUT);
                tokio::pin!(terminal);
                let outcome = loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        result = &mut terminal => break result,
                        _ = tick.tick() => {
                            // The producer can need our own withdrawal receipt before
                            // it can emit the terminal deployment event.
                            let _ = poll_consumer(
                                &node_name, &metrics_rx, &directory_rx, raft_to_api_offset,
                                &service_token, &cmd_tx, &shutdown, &cluster_http,
                                &mut receipt_cursor, io_timeout,
                            ).await;
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

            // The leader retains owners across rescheduling and local journal
            // loss. Its instructions therefore supplement our local inventory.
            let mut removed: std::collections::BTreeMap<_, Vec<LeaseRetirement>> = applied
                .keys()
                .filter(|key| !seen.contains(*key))
                .map(|key| (key.clone(), Vec::new()))
                .collect();
            for retirement in &assignments.retirements {
                let app = &retirement.placement.app_id;
                let key = (app.name.clone(), app.namespace.clone());
                if retirement.placement.node_id.0 != node_name || seen.contains(&key) {
                    eprintln!("orchestrator: refusing conflicting retirement instruction");
                    continue;
                }
                removed.entry(key).or_default().push(retirement.clone());
            }
            // Retirements run side by side: each may wait out a stubborn
            // workload's stop grace, and one must not hold up the rest.
            // Each future owns its inputs: a spawned task can't hold futures
            // that borrow from a closure's arguments.
            let mut retirements = futures_util::stream::iter(removed.into_iter().map(
                |((name, namespace), confirmations)| {
                    let cmd_tx = cmd_tx.clone();
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
                    _ = shutdown.cancelled() => return,
                    next = retirements.next() => next,
                };
                let Some((name, namespace, confirmations, retired)) = next else {
                    break;
                };
                match retired {
                    Ok(Ok(Ok(()))) => {
                        let mut next = applied.clone();
                        next.remove(&(name, namespace));
                        if let Err(error) =
                            persist_placements(checkpoint_path.as_deref(), &next).await
                        {
                            eprintln!("orchestrator: cannot record retirement: {error}");
                            continue;
                        }
                        applied = next;
                        for confirmation in confirmations {
                            let mut request = client
                                .post(format!("{leader_url}/v1/test/leases/retired"))
                                .json(&confirmation);
                            if let Some(token) = &service_token {
                                request = request.bearer_auth(token);
                            }
                            let acknowledged = tokio::select! {
                                _ = shutdown.cancelled() => return,
                                result = tokio::time::timeout(io_timeout, request.send()) => result,
                            };
                            if !matches!(acknowledged, Ok(Ok(ref response)) if response.status() == reqwest::StatusCode::NO_CONTENT)
                            {
                                eprintln!(
                                    "orchestrator: lease retirement acknowledgement failed; leader retains ownership"
                                );
                            }
                        }
                    }
                    Ok(Ok(Err(e))) => {
                        eprintln!(
                            "orchestrator: retirement of {name}/{namespace} failed, will retry: {e}"
                        );
                    }
                    Ok(Err(error)) => {
                        eprintln!(
                            "orchestrator: retirement of {name}/{namespace}: {error}; will retry"
                        );
                    }
                    Err(_) => {
                        eprintln!(
                            "orchestrator: retirement of {name}/{namespace} exceeded {retire_timeout:?}; ownership retained"
                        );
                    }
                }
            }
        }
    })
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

    #[tokio::test]
    async fn placement_deployment_waits_for_confirmed_cluster_publication() {
        let root = tempfile::tempdir().unwrap();
        let assignments = NodeAssignments {
            endpoint_generation: 7,
            apps: vec![NodeAssignment {
                name: "web".into(),
                namespace: "default".into(),
                replicas: 1,
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
                replicas: 1,
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
                        AgentCommand::SyncClusterConsumer { response, .. } => { let _ = response.send(Ok(crate::bun::agent::ConsumerUpdate { published: true, receipts: vec![] })); }
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
                replicas: 1,
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
                replicas: 1,
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

    fn placed_on(names: &[&str]) -> Vec<crate::meat::types::Placement> {
        names
            .iter()
            .map(|name| crate::meat::types::Placement {
                node_id: NodeId::new(*name),
                resources: Resources::new(100, 0, 0),
            })
            .collect()
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

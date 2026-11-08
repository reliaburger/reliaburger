//! `GET /v1/cluster/summary`, `GET /v1/cluster/capacity` and the two shares
//! the summary is assembled from.
//!
//! The node a client asks gathers three things at once, each under a
//! deadline: its own share and every peer's (`/v1/cluster/summary/node`),
//! the leader's view (`/v1/cluster/summary/leader`, read in place on the
//! leader), and gossip's member list. [`crate::bun::summary::assemble`]
//! turns them into the answer; nothing here decides a section's source.

use std::collections::HashSet;

use super::*;
use crate::bun::summary::{
    AppInstances, ClusterCapacity, FaultBrief, GitOpsSection, GossipCounters, JobRun, JobRunState,
    LeaderView, NodeCapacity, NodeCouncilView, NodeSummary, NodeUsage, SummaryInputs, Visibility,
};

/// How long the assembling node waits for each peer's share and for the
/// leader's view. `relish status` has a one second budget end to end, so
/// a node that hasn't answered by then is reported, not waited for.
pub(super) const SUMMARY_PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(700);

/// How many peers the assembling node asks at once.
const SUMMARY_FAN_OUT: usize = 32;

/// `GET /v1/cluster/summary`: the whole cluster at a glance.
pub(super) async fn cluster_summary_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    known: Option<axum::Extension<KnownMembers>>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    gossip: Option<axum::Extension<crate::mustard::stats::GossipStats>>,
    identity: Option<axum::Extension<crate::sesame::credentials::LiveNodeIdentity>>,
) -> Response {
    let sources = SummarySources {
        known: known.as_deref(),
        directory: directory.as_deref(),
        gossip: gossip.as_deref(),
        identity: identity.as_deref(),
    };
    Json(gather_cluster_summary(&state, auth.as_deref(), sources).await).into_response()
}

/// What a node reads the summary from besides its state: the request
/// extensions the router layers in. Each is absent on a standalone node or
/// in a test that doesn't set it.
#[derive(Clone, Copy)]
pub(super) struct SummarySources<'a> {
    pub(super) known: Option<&'a KnownMembers>,
    pub(super) directory: Option<&'a LeaderDirectory>,
    pub(super) gossip: Option<&'a crate::mustard::stats::GossipStats>,
    pub(super) identity: Option<&'a crate::sesame::credentials::LiveNodeIdentity>,
}

/// Assemble the summary as `auth` may see it. The summary route and
/// Brioche's dashboard both call this, so the page and `relish status`
/// read one answer.
pub(super) async fn gather_cluster_summary(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    sources: SummarySources<'_>,
) -> crate::bun::summary::ClusterSummary {
    let SummarySources {
        known,
        directory,
        gossip,
        identity,
    } = sources;
    let scoped = crate::sesame::auth::require_unscoped(auth).is_err();
    let metrics = !scoped
        && enforce_cluster_permission(state, auth, crate::config::PermissionAction::Metrics)
            .await
            .is_ok();
    let (local, (peers, unanswered), leader, members) = tokio::join!(
        local_node_summary(state, identity),
        peer_node_summaries(state),
        leader_view_from_leader(state, directory),
        cluster_members(state, known),
    );
    let mut shares = vec![local];
    shares.extend(peers);
    let leader = leader.map(|view| scope_leader_view(view, auth));
    for share in &mut shares {
        scope_node_summary(share, auth);
    }
    crate::bun::summary::assemble(&SummaryInputs {
        now_unix_ms: system_time_millis(std::time::SystemTime::now()),
        answered_by: local_node_name(state),
        version: running_version(state),
        visibility: Visibility { scoped, metrics },
        members,
        gossip: gossip.map(|stats| GossipCounters {
            updates_received: stats.updates_received(),
            last_change_unix_ms: stats.last_change_unix_ms(),
        }),
        shares,
        unanswered,
        leader,
    })
}

/// `GET /v1/cluster/summary/node`: this node's share of the summary. The
/// assembling node asks for it under the service token; any unscoped
/// caller may read it, since it holds counts, never names of secrets.
pub(super) async fn node_summary_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    identity: Option<axum::Extension<crate::sesame::credentials::LiveNodeIdentity>>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    Json(local_node_summary(&state, identity.as_deref()).await).into_response()
}

/// `GET /v1/cluster/summary/leader`: the leader's view. Only a node that
/// leads, or a standalone node, answers; a follower refuses rather than
/// forward, because its caller is already the forwarder.
pub(super) async fn leader_view_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    match local_leader_view(&state).await {
        Ok(view) => Json(view).into_response(),
        Err(error) => unavailable_response(error),
    }
}

/// `GET /v1/cluster/capacity`: what the scheduler has committed on each
/// node. Any node answers, asking the leader when it isn't one. A scoped
/// token is refused: the totals cover every tenant.
pub(super) async fn cluster_capacity_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    match leader_view_from_leader(&state, directory.as_deref()).await {
        Ok(LeaderView {
            capacity: Some(capacity),
            ..
        }) => Json(capacity).into_response(),
        Ok(_) => unavailable_response("a standalone node keeps no state reports".to_string()),
        Err(error) => unavailable_response(error),
    }
}

/// The version this node runs.
fn running_version(state: &ApiState) -> String {
    match &state.upgrade {
        Some(manager) => manager.running_version().to_string(),
        None => crate::upgrade::version::compiled_version().to_string(),
    }
}

/// Every member gossip knows, down ones included, as `/v1/cluster/nodes`
/// lists them.
pub(super) async fn cluster_members(
    state: &ApiState,
    known: Option<&KnownMembers>,
) -> Result<Vec<crate::bun::agent::NodeStatus>, String> {
    let down = match known {
        Some(known) => known
            .down()
            .await
            .into_iter()
            .map(|member| crate::bun::agent::NodeStatus {
                node_id: member.info.node_id.0.clone(),
                address: member.gossip_address.to_string(),
                api_address: member.info.api_advertised.then_some(member.info.address),
                state: member.state.to_string(),
                incarnation: member.incarnation,
                is_council: false,
                is_leader: false,
                labels: member.labels,
            })
            .collect(),
        None => Vec::new(),
    };
    let mut nodes = ask_agent_bounded(&state.cmd_tx, |response| AgentCommand::Nodes {
        down,
        response,
    })
    .await
    .map_err(|_| "the agent didn't list the members in time".to_string())?;
    if let Some(membership) = &state.membership {
        let members = membership.read().await;
        // A down row already carries its last advertised address, and the
        // live table has none for it.
        for node in nodes.iter_mut().filter(|n| n.api_address.is_none()) {
            node.api_address = members
                .iter()
                .find(|member| member.node_id.0 == node.node_id && member.api_advertised)
                .map(|member| member.address);
        }
    }
    Ok(nodes)
}

/// Every live peer's share, and each peer that didn't answer with why.
async fn peer_node_summaries(state: &ApiState) -> (Vec<NodeSummary>, Vec<(String, String)>) {
    let local_name = local_node_name(state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| async move {
                let answer = fetch_from_peer::<NodeSummary>(
                    state,
                    &member,
                    "/v1/cluster/summary/node",
                    SUMMARY_PEER_TIMEOUT,
                )
                .await;
                (member.node_id.0, answer)
            }),
    )
    .buffer_unordered(SUMMARY_FAN_OUT);
    tokio::pin!(requests);
    let mut shares = Vec::new();
    let mut unanswered = Vec::new();
    while let Some((name, answer)) = requests.next().await {
        match answer {
            Ok(share) => shares.push(share),
            Err(error) => {
                // `fetch_from_peer` names the node; the summary names it once.
                let reason = error
                    .strip_prefix(&format!("node {name}: "))
                    .or_else(|| error.strip_prefix(&format!("node {name} ")))
                    .unwrap_or(&error)
                    .to_string();
                unanswered.push((name, reason));
            }
        }
    }
    shares.sort_by(|a, b| a.node.cmp(&b.node));
    unanswered.sort();
    (shares, unanswered)
}

/// This node's share, read from its own stores. Every read is local and
/// bounded; one that can't be made leaves its field `None`.
pub(super) async fn local_node_summary(
    state: &ApiState,
    identity: Option<&crate::sesame::credentials::LiveNodeIdentity>,
) -> NodeSummary {
    let node = local_node_name(state);
    let storage_paths = state.static_capabilities.diagnostics.storage_paths.clone();
    let disks = tokio::task::spawn_blocking(move || {
        crate::bun::diagnostics::collect_disk_usage(&storage_paths, 0)
    });
    let (apps, faults) = tokio::join!(local_app_instances(state), local_faults(state));
    let newest_log_unix_ms = match &state.log_store {
        Some(store) => store.read().await.newest_line_unix_ms(),
        None => None,
    };
    let usage = match &state.mayo {
        Some(store) => node_usage(&*store.read().await),
        None => NodeUsage::default(),
    };
    let alerts = match &state.alerts {
        Some(alerts) => alerts
            .read()
            .await
            .all_statuses()
            .into_iter()
            .filter(|alert| alert.state == crate::mayo::alert::AlertPhase::Firing)
            .map(|alert| format!("{} on {node}", alert.rule_name))
            .collect(),
        None => Vec::new(),
    };
    let certificate_not_after = match identity {
        Some(identity) => Some(system_time_seconds(identity.snapshot().not_after)),
        None => state
            .static_capabilities
            .diagnostics
            .node_certificate
            .as_ref()
            .map(|certificate| certificate.not_after),
    };
    let data_disk_percent = match disks.await {
        Ok(
            crate::bun::diagnostics::DiagnosticSource::Available { value, .. }
            | crate::bun::diagnostics::DiagnosticSource::Degraded { value, .. },
        ) => value
            .iter()
            .map(|disk| disk.used_percent)
            .max_by(f64::total_cmp),
        _ => None,
    };
    NodeSummary {
        version: running_version(state),
        council: local_council_view(state),
        apps,
        newest_log_unix_ms,
        usage,
        data_disk_percent,
        faults,
        alerts,
        certificate_not_after,
        node,
    }
}

fn system_time_seconds(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// This node's view of the council, from its Raft metrics alone: no
/// desired-state read and no quorum round trip.
fn local_council_view(state: &ApiState) -> NodeCouncilView {
    let Some(council) = &state.council else {
        return NodeCouncilView::default();
    };
    // Copied out in one statement, so the watch borrow ends here.
    let metrics = council.metrics().borrow().clone();
    let membership = metrics.membership_config.membership();
    let name = |id: u64| membership.get_node(&id).map(|node| node.name.clone());
    NodeCouncilView {
        leader: metrics.current_leader.and_then(name),
        term: metrics.current_term,
        last_applied: metrics.last_applied.map(|log| log.index),
        voters: membership.voter_ids().filter_map(name).collect(),
        fenced: false,
    }
}

/// This node's instances, counted per app.
async fn local_app_instances(state: &ApiState) -> Vec<AppInstances> {
    let statuses = match local_statuses(state).await {
        Ok(statuses) => statuses,
        Err(_) => return Vec::new(),
    };
    let mut apps: std::collections::BTreeMap<(String, String), AppInstances> = Default::default();
    for status in statuses {
        let entry = apps
            .entry((status.namespace.clone(), status.app_name.clone()))
            .or_insert_with(|| AppInstances {
                app: status.app_name.clone(),
                namespace: status.namespace.clone(),
                running: 0,
                not_running: 0,
            });
        if status.state == "running" {
            entry.running += 1;
        } else {
            entry.not_running += 1;
        }
    }
    apps.into_values().collect()
}

/// The Smoker faults this node holds.
async fn local_faults(state: &ApiState) -> Vec<FaultBrief> {
    let listed = tokio::time::timeout(
        SUMMARY_PEER_TIMEOUT,
        ask_agent(&state.cmd_tx, |response| AgentCommand::ListFaults {
            response,
        }),
    )
    .await;
    let Ok(Ok(faults)) = listed else {
        return Vec::new();
    };
    faults
        .into_iter()
        .map(|fault| FaultBrief {
            kind: fault.fault_type,
            target: match fault.target_node {
                Some(node) => format!("node {node}"),
                None => fault.target_service,
            },
            namespace: fault.namespace,
            remaining_seconds: fault.remaining_secs,
        })
        .collect()
}

/// The newest node-level samples in this node's metrics store.
fn node_usage(store: &MayoStore) -> NodeUsage {
    let value = |name: &str| store.latest_node_sample(name).map(|sample| sample.value);
    NodeUsage {
        newest_sample_unix_ms: store
            .newest_node_sample_time()
            .map(|seconds| seconds.saturating_mul(1000)),
        cpu_percent: value("node_cpu_usage_percent"),
        memory_used_bytes: value("node_memory_used_bytes"),
        memory_total_bytes: value("node_memory_total_bytes"),
        disk_used_bytes: value("node_disk_used_bytes"),
        disk_total_bytes: value("node_disk_total_bytes"),
    }
}

/// The leader's view: read in place when this node leads (or runs alone),
/// asked of the leader otherwise.
async fn leader_view_from_leader(
    state: &ApiState,
    directory: Option<&LeaderDirectory>,
) -> Result<LeaderView, String> {
    let Some(council) = &state.council else {
        return local_leader_view(state).await;
    };
    let leader_url =
        match super::upgrade::leader_base_url(state, council, directory, &HeaderMap::new()).await {
            Ok(url) => url,
            Err(NoLeaderRoute::ThisNode) => return local_leader_view(state).await,
            Err(NoLeaderRoute::Unknown) => return Err("no leader is known yet".to_string()),
            Err(other) => return Err(other.to_string()),
        };
    let mut request = mark_leader_forwarded(
        state
            .cluster_http
            .client()
            .get(format!("{leader_url}/v1/cluster/summary/leader")),
    );
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    let exchange = async {
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("the leader answered {status}: {body}"));
        }
        response
            .json::<LeaderView>()
            .await
            .map_err(|error| format!("the leader's answer was unreadable: {error}"))
    };
    tokio::time::timeout(SUMMARY_PEER_TIMEOUT, exchange)
        .await
        .map_err(|_| "the leader didn't answer in time".to_string())?
}

/// The leader's view, computed here. A clustered node that can't confirm
/// it leads refuses: its desired state may be stale or empty.
async fn local_leader_view(state: &ApiState) -> Result<LeaderView, String> {
    let Some(council) = &state.council else {
        let apps = gather_desired_apps(state, DesiredAppsSource::ForwardedRequest).await?;
        return Ok(LeaderView {
            capacity: None,
            apps,
            upgrade_in_progress: false,
            gitops: GitOpsSection {
                configured: false,
                commit: None,
                synced_at_unix_seconds: None,
                error: None,
            },
            jobs: job_runs(state).await,
        });
    };
    if !confirmed_lease_leader(council).await {
        return Err("this node can't confirm it leads; retry once the election settles".into());
    }
    let desired = council.desired_state().await;
    let live: Vec<crate::meat::NodeId> = match &state.membership {
        Some(membership) => membership
            .read()
            .await
            .iter()
            .map(|member| member.node_id.clone())
            .collect(),
        None => Vec::new(),
    };
    let mut live_names: HashSet<String> = live.iter().map(|node| node.0.clone()).collect();
    live_names.insert(local_node_name(state));
    let apps = super::node_info::council_app_evidence(&desired, Some(&live_names));
    let capacity = state.aggregated_rx.as_ref().map(|reports| {
        let reports = reports.borrow();
        capacity_from_reports(&live_names, &reports, desired.active_upgrade.as_ref())
    });
    let gitops = GitOpsSection {
        configured: desired.gitops_config.is_some(),
        commit: desired
            .gitops_sync_state
            .as_ref()
            .and_then(|sync| sync.last_applied_commit.as_ref())
            .map(|commit| commit.sha.clone()),
        synced_at_unix_seconds: desired
            .gitops_sync_state
            .as_ref()
            .and_then(|sync| sync.last_sync_at)
            .map(|unix_ms| unix_ms / 1000),
        error: desired
            .gitops_sync_state
            .as_ref()
            .and_then(|sync| sync.last_error.clone()),
    };
    Ok(LeaderView {
        capacity,
        apps,
        upgrade_in_progress: desired.active_upgrade.is_some(),
        gitops,
        jobs: job_runs(state).await,
    })
}

/// Each live node's committed resources from its fresh state report.
/// Nodes with no fresh report are named, never counted as zero.
pub(super) fn capacity_from_reports(
    live: &HashSet<String>,
    reports: &crate::reporting::aggregator::AggregatedState,
    upgrade: Option<&crate::upgrade::types::ClusterUpgradeState>,
) -> ClusterCapacity {
    let mut capacity = ClusterCapacity::default();
    let mut names: Vec<&String> = live.iter().collect();
    names.sort();
    for name in names {
        let node = crate::meat::NodeId::new(name);
        let report = reports
            .reports
            .get(&node)
            .filter(|_| reports.report_is_fresh(&node));
        let Some(report) = report else {
            capacity.not_reporting.push(name.clone());
            continue;
        };
        let usage = &report.resource_usage;
        capacity.nodes.push(NodeCapacity {
            node: name.clone(),
            cpu_allocated_millicores: u64::from(usage.cpu_used_millicores)
                .saturating_add(report.job_reservations.cpu_millicores),
            cpu_total_millicores: u64::from(usage.cpu_total_millicores),
            memory_allocated_mb: u64::from(usage.memory_used_mb)
                .saturating_add(report.job_reservations.memory_bytes / (1024 * 1024)),
            memory_total_mb: u64::from(usage.memory_total_mb),
            gpu_allocated: usage.gpu_used,
            gpu_total: usage.gpu_total,
            cordoned: upgrade.is_some_and(|upgrade| upgrade.is_node_cordoned(name)),
        });
    }
    capacity
}

/// Every job run and scheduled definition the cluster keeps.
async fn job_runs(state: &ApiState) -> Vec<JobRun> {
    use crate::meat::task_array_state::TaskArrayStatus;
    let arrays = crate::bun::task_array_leader::read_task_arrays(state).await;
    let mut runs = Vec::new();
    for (id, run) in arrays.jobs().runs() {
        let Some(record) = arrays.get(id) else {
            continue;
        };
        let summary = record.state.summary();
        let state = match summary.status {
            TaskArrayStatus::Running | TaskArrayStatus::Stopping => JobRunState::Running,
            TaskArrayStatus::Succeeded => JobRunState::Succeeded,
            TaskArrayStatus::Failed | TaskArrayStatus::CompletedWithFailures => JobRunState::Failed,
            TaskArrayStatus::Cancelled => JobRunState::Cancelled,
        };
        runs.push(JobRun {
            name: run.name.clone(),
            namespace: run.namespace.clone(),
            state,
            tasks_queued: summary.queued,
            tasks_running: summary.held,
            finished_at_unix_seconds: record.terminal_at_epoch_secs,
        });
    }
    for (key, record) in arrays.jobs().definitions() {
        if record.definition.cron.is_none() {
            continue;
        }
        let Some((namespace, name)) = key.split_once('/') else {
            continue;
        };
        runs.push(JobRun {
            name: name.to_string(),
            namespace: namespace.to_string(),
            state: JobRunState::Scheduled,
            tasks_queued: 0,
            tasks_running: 0,
            finished_at_unix_seconds: None,
        });
    }
    runs
}

/// Trim the leader's view to what `auth` may see.
fn scope_leader_view(
    mut view: LeaderView,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> LeaderView {
    let visible = |app: &str, namespace: &str| {
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
    };
    view.apps = super::node_info::filter_desired_apps_for_scope(view.apps, auth);
    view.jobs.retain(|run| visible(&run.name, &run.namespace));
    view
}

/// Trim one node's share to what `auth` may see.
fn scope_node_summary(share: &mut NodeSummary, auth: Option<&crate::sesame::auth::AuthContext>) {
    if crate::sesame::auth::require_unscoped(auth).is_ok() {
        return;
    }
    let visible = |app: &str, namespace: &str| {
        crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok()
    };
    share.apps.retain(|app| visible(&app.app, &app.namespace));
    share.faults.retain(|fault| {
        fault
            .namespace
            .as_deref()
            .is_some_and(|namespace| visible(&fault.target, namespace))
    });
    // Alerts and usage are cluster metrics; `assemble` drops those
    // sections for a scoped token, and these never leave the node.
    share.alerts.clear();
    share.usage = NodeUsage::default();
    share.newest_log_unix_ms = None;
}

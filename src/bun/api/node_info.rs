//! Node information routes: health, readiness, version, capabilities,
//! diagnostics, desired apps and the path probe.

use std::collections::HashSet;

use super::*;

/// Liveness check.
pub(super) async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Return live critical-subsystem evidence. Unlike `/v1/health`, this is an
/// authenticated scheduling signal and returns 503 while the node is fenced.
pub(super) async fn readiness_handler(State(state): State<ApiState>) -> Response {
    let evidence = state.readiness.snapshot().await;
    let status = if evidence.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(evidence)).into_response()
}

/// Report the running binary version (public, dependency-free, fast).
///
/// The upgrade orchestrator polls this to decide whether a node has
/// reached its target version, so it must answer even when the agent
/// loop is busy — hence direct manager access, not an AgentCommand.
/// `GET /v1/capabilities` — what this node has wired up.
///
/// The `Option` fields on `ApiState` are the source of truth for the
/// subsystems: a `None` there means the subsystem was never built, which is
/// exactly what a caller needs to distinguish from "built but failing".
pub(super) async fn capabilities_handler(State(state): State<ApiState>) -> impl IntoResponse {
    Json(local_capability_report(&state).await)
}

pub(super) async fn local_capability_report(
    state: &ApiState,
) -> crate::bun::capabilities::ClusterCapabilities {
    let wired = crate::bun::capabilities::WiredSubsystems {
        metrics: state.mayo.is_some(),
        logs: state.log_store.is_some(),
        rollups: state.rollup_store.is_some(),
        council: state.council.is_some(),
        registry: state.pickle_catalog.is_some(),
        events: state.events.is_some(),
        upgrade: state.upgrade.is_some(),
        member_count: match &state.membership {
            Some(members) => Some(members.read().await.len() as u32),
            None => None,
        },
    };
    let (readiness, placement) = state.readiness.snapshots().await;
    let gossip_fresh = readiness.subsystems.iter().any(|subsystem| {
        subsystem.name == "cluster:gossip"
            && subsystem.state == crate::bun::readiness::SubsystemState::Ready
    });
    let raft_fresh = readiness.subsystems.iter().any(|subsystem| {
        subsystem.name == "cluster:raft-rpc"
            && subsystem.state == crate::bun::readiness::SubsystemState::Ready
    });
    let members = match &state.membership {
        Some(membership) if !state.static_capabilities.cluster_mode || gossip_fresh => {
            Some(membership.read().await.clone())
        }
        Some(_) => None,
        None if state.static_capabilities.cluster_mode => None,
        None => Some(Vec::new()),
    };
    let membership_count = members.as_ref().map(|members| {
        let includes_self = members
            .iter()
            .any(|member| member.node_id.0 == state.static_capabilities.node_id);
        (members.len() + usize::from(!includes_self))
            .try_into()
            .unwrap_or(u32::MAX)
    });
    let council_quorum = match (&state.council, &members) {
        (Some(council), Some(members)) if raft_fresh => Some(council_has_live_quorum(
            council,
            members,
            &state.static_capabilities.node_id,
        )),
        (None, _) if !state.static_capabilities.cluster_mode => Some(false),
        _ => None,
    };
    let cluster_id = match &state.council {
        Some(council) => {
            let security = council.security_state().await;
            security
                .get_ca(crate::sesame::types::CaRole::Root)
                .map(|root| {
                    let digest = ring::digest::digest(&ring::digest::SHA256, &root.certificate_der);
                    format!("sha256:{}", hex::encode(digest.as_ref()))
                })
        }
        None => None,
    };
    let mut wired = wired;
    wired.member_count = membership_count;
    crate::bun::capabilities::ClusterCapabilities::derive_with_observations(
        &state.static_capabilities,
        &wired,
        crate::bun::capabilities::CapabilityObservations {
            readiness: Some(readiness),
            placement: Some(placement),
            cluster_id,
            council_quorum,
        },
    )
}

pub(super) fn council_has_live_quorum(
    council: &crate::council::CouncilNode,
    members: &[NodeMembershipInfo],
    self_node_id: &str,
) -> bool {
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<u64> =
        metrics.membership_config.membership().voter_ids().collect();
    if voters.is_empty() || metrics.current_leader.is_none() {
        return false;
    }
    let live: std::collections::BTreeSet<u64> = members
        .iter()
        .map(|member| crate::cluster::identity::raft_id_from_name(&member.node_id.0))
        .chain(std::iter::once(
            crate::cluster::identity::raft_id_from_name(self_node_id),
        ))
        .collect();
    voters.iter().filter(|id| live.contains(id)).count() > voters.len() / 2
}

pub(super) async fn cluster_capabilities_handler(
    State(state): State<ApiState>,
) -> Json<crate::bun::capabilities::ClusterCapabilityReport> {
    let started = std::time::SystemTime::now();
    let deadline =
        tokio::time::Instant::now() + crate::bun::capabilities::CLUSTER_COLLECTION_TIMEOUT;
    let local = local_capability_report(&state).await;
    let mut nodes = vec![
        crate::bun::capabilities::CollectedNodeCapability::Evidence {
            node_id: local.node_id.clone(),
            address: "local".to_string(),
            report: Box::new(local),
        },
    ];
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let peers = members
        .into_iter()
        .filter(|member| member.node_id.0 != state.static_capabilities.node_id)
        .map(|member| {
            let cluster_http = state.cluster_http.clone();
            let service_token = state.service_token.clone();
            async move {
                crate::bun::capabilities::collect_peer_capability(
                    &cluster_http,
                    service_token.as_deref(),
                    &member.node_id.0,
                    member.address,
                    deadline,
                )
                .await
            }
        });
    nodes.extend(futures_util::future::join_all(peers).await);
    nodes.sort_by(|left, right| {
        collected_capability_node_id(left).cmp(collected_capability_node_id(right))
    });

    Json(crate::bun::capabilities::ClusterCapabilityReport {
        schema_version: crate::bun::capabilities::CAPABILITY_SCHEMA_VERSION,
        collected_by: state.static_capabilities.node_id.clone(),
        observed_at_unix_ms: system_time_millis(started),
        deadline_at_unix_ms: system_time_millis(
            started + crate::bun::capabilities::CLUSTER_COLLECTION_TIMEOUT,
        ),
        nodes,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DiagnosticsQuery {
    pub(super) window_seconds: Option<u64>,
}

/// `GET /v1/diagnostics` — bounded local evidence for `relish wtf`.
///
/// CPU throttling is a delta between two cumulative cgroup samples. The
/// caller can request a 1–10 second window; one second is the default so the
/// endpoint cannot be turned into an arbitrarily long-lived request.
pub(super) async fn diagnostics_handler(
    live_identity: Option<axum::Extension<crate::sesame::credentials::LiveNodeIdentity>>,
    renewal: Option<axum::Extension<crate::sesame::renewal_worker::RenewalMonitor>>,
    State(state): State<ApiState>,
    Query(query): Query<DiagnosticsQuery>,
) -> Json<crate::bun::diagnostics::LocalDiagnosticSnapshot> {
    use crate::bun::diagnostics::{DiagnosticSource, LocalDiagnosticSnapshot};

    let window_seconds = query.window_seconds.unwrap_or(1).clamp(1, 10);
    let first_observed_at = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let storage_paths = state.static_capabilities.diagnostics.storage_paths.clone();
    let disk_task = tokio::task::spawn_blocking(move || {
        crate::bun::diagnostics::collect_disk_usage(&storage_paths, first_observed_at)
    });

    let cpu_throttling = if state.static_capabilities.cgroup_faults {
        let first_statuses = gather_statuses(&state).await;
        let first = crate::bun::diagnostics::collect_cpu_throttle_totals(
            &first_statuses,
            first_observed_at,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(window_seconds)).await;
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let second_statuses = gather_statuses(&state).await;
        let second =
            crate::bun::diagnostics::collect_cpu_throttle_totals(&second_statuses, observed_at)
                .await;
        crate::bun::diagnostics::cpu_throttle_window(first, second, window_seconds, observed_at)
    } else {
        DiagnosticSource::Unsupported {
            reason: "actual CPU throttled time requires rootful Linux cgroup v2".to_string(),
        }
    };

    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let disks = match disk_task.await {
        Ok(disks) => disks,
        Err(error) => DiagnosticSource::Unavailable {
            reason: format!("disk capacity collector failed: {error}"),
        },
    };
    let certificates = match live_identity {
        Some(identity) => {
            let current = identity.snapshot();
            let worker_state = renewal.as_ref().map(|monitor| monitor.state());
            let rotation_state = if std::time::SystemTime::now() >= current.not_after {
                "expired"
            } else {
                worker_state.map_or("manual", |state| state.as_str())
            };
            let automatic_rotation = worker_state
                .is_some_and(|state| state != crate::sesame::renewal_worker::RenewalState::Stopped);
            match crate::bun::diagnostics::public_certificate_metadata(
                "node",
                &current.node_id,
                &current.certificate_der,
                rotation_state,
                automatic_rotation,
            ) {
                Ok(metadata) => DiagnosticSource::Available {
                    observed_at,
                    value: vec![metadata],
                },
                Err(reason) => DiagnosticSource::Unavailable { reason },
            }
        }
        None => match &state.static_capabilities.diagnostics.node_certificate {
            Some(certificate) => DiagnosticSource::Available {
                observed_at,
                value: vec![certificate.clone()],
            },
            None if !state.static_capabilities.identity => DiagnosticSource::Unsupported {
                reason: "workload identity issuance is disabled and no node mTLS leaf is loaded"
                    .to_string(),
            },
            None => DiagnosticSource::Unavailable {
                reason:
                    "identity issuance is enabled but no safe certificate inventory is available"
                        .to_string(),
            },
        },
    };

    Json(LocalDiagnosticSnapshot {
        schema_version: crate::bun::diagnostics::LOCAL_DIAGNOSTIC_SCHEMA_VERSION,
        node_id: state.static_capabilities.node_id.clone(),
        observed_at,
        disks,
        cpu_throttling,
        certificates,
    })
}

/// `GET /v1/diagnostics/apps` — desired replicas and scheduler coverage.
pub(super) async fn desired_apps_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
) -> Response {
    let source = if headers.contains_key(DESIRED_APPS_FORWARDED_HEADER) {
        DesiredAppsSource::ForwardedRequest
    } else {
        DesiredAppsSource::Caller(directory.as_deref())
    };
    match gather_desired_apps(&state, source).await {
        Ok(apps) => Json(filter_desired_apps_for_scope(apps, auth.as_deref())).into_response(),
        Err(error) => unavailable_response(error),
    }
}

/// Desired replicas, placements, any quota block and any volume home it
/// waits for, for every app the council knows, sorted by namespace and name.
///
/// `live` names the cluster's live members; `None` when this node can't
/// tell, and then no app is reported as waiting for its volume home.
pub(super) fn council_app_evidence(
    desired: &crate::council::types::DesiredState,
    live: Option<&HashSet<String>>,
) -> Vec<crate::bun::diagnostics::DesiredAppEvidence> {
    let live_nodes = live.map_or(1, |live| live.len().max(1));
    let mut apps = desired
        .apps
        .iter()
        .map(
            |(app_id, spec)| crate::bun::diagnostics::DesiredAppEvidence {
                app: app_id.name.clone(),
                namespace: app_id.namespace.clone(),
                desired_replicas: crate::bun::diagnostics::desired_replica_count(
                    spec.replicas,
                    live_nodes,
                ),
                scheduled_replicas: desired.scheduling.get(app_id).map_or(0, |placements| {
                    placements.len().try_into().unwrap_or(u32::MAX)
                }),
                placements: desired.scheduling.get(app_id).map_or_else(
                    Default::default,
                    |placements| {
                        let mut per_node = std::collections::BTreeMap::new();
                        for placement in placements {
                            *per_node.entry(placement.node_id.0.clone()).or_insert(0u32) += 1;
                        }
                        per_node
                    },
                ),
                service_port: spec.port,
                blocked: desired.quota_blocked.get(app_id).cloned(),
                volume_home_away: live.and_then(|live| {
                    crate::cluster::orchestrate::volume_home_away(desired, app_id, spec, live)
                        .map(|node| node.0)
                }),
            },
        )
        .collect::<Vec<_>>();
    apps.sort_by(|left, right| (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app)));
    apps
}

/// Marks a desired-apps read a non-leader has already forwarded, so two nodes
/// that disagree about the leader can't pass it back and forth.
pub(super) const DESIRED_APPS_FORWARDED_HEADER: &str = "x-reliaburger-desired-apps-forwarded";

/// How long a non-leader waits for the leader's desired-apps answer.
const DESIRED_APPS_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Who is asking for desired apps, which decides what a non-leader may do.
#[derive(Clone, Copy)]
pub(super) enum DesiredAppsSource<'a> {
    /// A caller on this node (CLI, dashboard). A non-leader forwards to the
    /// leader, found through the gossip directory when there is one.
    Caller(Option<&'a LeaderDirectory>),
    /// Another node already forwarded this read; never forward it again.
    ForwardedRequest,
}

/// Desired replicas and scheduler coverage, from the current leader.
///
/// Only a leader that still holds a quorum answers from its own state. A
/// worker outside Raft has an empty, unreplicated state machine, so its own
/// view would claim the cluster runs nothing (#436). Any other node forwards
/// the read to the leader, so `relish status` and the dashboard work from
/// every node, and reports an error when no leader answers.
pub(super) async fn gather_desired_apps(
    state: &ApiState,
    source: DesiredAppsSource<'_>,
) -> Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, String> {
    let apps = if let Some(council) = &state.council {
        if !confirmed_lease_leader(council).await {
            return match source {
                DesiredAppsSource::Caller(directory) => {
                    desired_apps_from_leader(state, council, directory).await
                }
                DesiredAppsSource::ForwardedRequest => Err(
                    "this node was named the leader but isn't; retry once the election settles"
                        .into(),
                ),
            };
        }
        let desired = council.desired_state().await;
        let live = match &state.membership {
            Some(membership) => {
                let mut live: HashSet<String> = membership
                    .read()
                    .await
                    .iter()
                    .map(|member| member.node_id.0.clone())
                    .collect();
                // The node answering is live, whether or not its own table
                // lists it.
                if !state.static_capabilities.node_id.is_empty() {
                    live.insert(state.static_capabilities.node_id.clone());
                }
                Some(live)
            }
            None => None,
        };
        council_app_evidence(&desired, live.as_ref())
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (response, receiver) = oneshot::channel();
            state
                .cmd_tx
                .send(AgentCommand::DesiredApps { response })
                .await
                .map_err(|_| "agent unavailable".to_string())?;
            receiver
                .await
                .map_err(|_| "agent dropped desired-app response".to_string())
        })
        .await
        .map_err(|_| "desired-app query timed out".to_string())??
    };
    Ok(apps)
}

/// Ask the leader's API for its desired apps.
async fn desired_apps_from_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    directory: Option<&LeaderDirectory>,
) -> Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, String> {
    let advertised = directory.and_then(|LeaderDirectory(directory)| {
        let metrics = council.metrics();
        let metrics = metrics.borrow();
        crate::cluster::directory::leader_api_address(&metrics, &directory.borrow())
    });
    let leader_url = match advertised {
        Some(address) => state.cluster_http.url(&address.to_string(), ""),
        None => leader_api_url(state, council).await.ok_or_else(|| {
            "desired cluster state requires a current leader, and none is known yet; retry shortly"
                .to_string()
        })?,
    };
    let mut request = state
        .cluster_http
        .client()
        .get(format!("{leader_url}/v1/diagnostics/apps"))
        .header(DESIRED_APPS_FORWARDED_HEADER, "1");
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
            .json::<Vec<crate::bun::diagnostics::DesiredAppEvidence>>()
            .await
            .map_err(|error| format!("the leader's answer was unreadable: {error}"))
    };
    tokio::time::timeout(DESIRED_APPS_FORWARD_TIMEOUT, exchange)
        .await
        .map_err(|_| "the leader did not answer the desired-apps read in time".to_string())?
        .map_err(|error| format!("could not read desired apps from the leader: {error}"))
}

/// `POST /v1/path` — fixed DNS and TCP probes from a local source workload.
pub(super) async fn path_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Json(mut request): Json<crate::onion::trace::TraceRequest>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    if request.port == Some(0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "path destination port must be between 1 and 65535"})),
        )
            .into_response();
    }
    if request
        .count
        .is_some_and(|count| count == 0 || count > crate::onion::trace::MAX_TRACE_CONNECTS)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "path probe count must be between 1 and {}",
                crate::onion::trace::MAX_TRACE_CONNECTS
            )})),
        )
            .into_response();
    }
    if !valid_path_label(&request.source) || !valid_path_label(&request.source_namespace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "path source and namespace must be DNS labels"})),
        )
            .into_response();
    }
    if let Err(response) = crate::sesame::auth::authorize_scoped(
        auth.as_deref(),
        &request.source,
        &request.source_namespace,
    ) {
        return response;
    }

    let internal_destination = valid_path_label(&request.destination);
    if internal_destination {
        if !valid_path_label(&request.destination_namespace) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "internal destination namespace must be a DNS label"})),
            )
                .into_response();
        }
        if let Err(response) = crate::sesame::auth::authorize_scoped(
            auth.as_deref(),
            &request.destination,
            &request.destination_namespace,
        ) {
            return response;
        }
        let (sender, receiver) = oneshot::channel();
        if state
            .cmd_tx
            .send(AgentCommand::ResolveAll { response: sender })
            .await
            .is_err()
        {
            return (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response();
        }
        let services = match receiver.await {
            Ok(services) => services,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agent dropped service-map response",
                )
                    .into_response();
            }
        };
        let Some(service) = services.iter().find(|service| {
            service.app_name == request.destination
                && service.namespace == request.destination_namespace
        }) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "internal destination is absent from the live service map"})),
            )
                .into_response();
        };
        request.port.get_or_insert(service.port);
    } else {
        let Some(port) = request.port else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "external path destination requires --port"})),
            )
                .into_response();
        };
        let Some(auth) = auth.as_deref() else {
            return (
                StatusCode::FORBIDDEN,
                "an external path requires an authenticated Admin credential",
            )
                .into_response();
        };
        if let Err(error) = state.static_capabilities.test_policy.authorise(
            crate::testkit::safety::OperationPermission::ProbeExternalDestination,
            &crate::testkit::safety::OperationAuthorisation {
                principal: &auth.principal_id,
                role: auth.role,
                acknowledged: false,
            },
        ) {
            return (StatusCode::FORBIDDEN, error.to_string()).into_response();
        }
        if !state
            .static_capabilities
            .test_policy
            .permits_external_probe(&request.destination, port)
        {
            return (
                StatusCode::FORBIDDEN,
                "external path destination is not exactly allowlisted as host:port",
            )
                .into_response();
        }
    }

    let (sender, receiver) = oneshot::channel();
    if state
        .cmd_tx
        .send(AgentCommand::Trace {
            request,
            internal_destination,
            source_node: state.static_capabilities.node_id.clone(),
            response: sender,
        })
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response();
    }
    // DNS (8s) plus up to ten connects at three seconds each.
    match tokio::time::timeout(std::time::Duration::from_secs(45), receiver).await {
        Ok(Ok(Ok(result))) => Json(result).into_response(),
        Ok(Ok(Err(crate::bun::BunError::AppNotFound { .. }))) => (
            StatusCode::NOT_FOUND,
            "source app has no running instance on this node",
        )
            .into_response(),
        Ok(Ok(Err(crate::bun::BunError::TraceBusy))) => (
            StatusCode::TOO_MANY_REQUESTS,
            "too many path probes are already running on this node",
        )
            .into_response(),
        Ok(Ok(Err(error))) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
        Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "agent dropped response").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "path probe timed out after 45 seconds",
        )
            .into_response(),
    }
}

pub(super) fn valid_path_label(value: &str) -> bool {
    crate::config::valid_workload_label(value)
}

pub(super) fn filter_desired_apps_for_scope(
    mut apps: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Vec<crate::bun::diagnostics::DesiredAppEvidence> {
    apps.retain(|app| {
        crate::sesame::auth::authorize_scoped(auth, &app.app, &app.namespace).is_ok()
    });
    apps
}

pub(super) fn collected_capability_node_id(
    entry: &crate::bun::capabilities::CollectedNodeCapability,
) -> &str {
    match entry {
        crate::bun::capabilities::CollectedNodeCapability::Evidence { node_id, .. }
        | crate::bun::capabilities::CollectedNodeCapability::Unknown { node_id, .. } => node_id,
    }
}

/// Report the running binary version (public, dependency-free, fast).
///
/// The upgrade orchestrator polls this to decide whether a node has
/// reached its target version, so it must answer even when the agent
/// loop is busy — hence direct manager access, not an AgentCommand.
pub(super) async fn version_handler(State(state): State<ApiState>) -> impl IntoResponse {
    let mut version = version_body(&state).await;
    // The appliance OS (W6): what's running and the update in progress, so
    // the OS rollout can tell a node that updated from one that fell back.
    if let Some(slot) = crate::os::slot::installed() {
        version["os_version"] = serde_json::json!(slot.running());
        version["os_update"] = serde_json::json!(slot.state().await);
    }
    Json(version)
}

async fn version_body(state: &ApiState) -> serde_json::Value {
    match &state.upgrade {
        Some(manager) => serde_json::json!({
            "version": manager.running_version().to_string(),
            // The version alone doesn't identify the bytes: the upgrade
            // start gate and the orchestrator compare this digest with the
            // candidate's so a same-version build can't pass as a swap.
            "binary_sha256": manager.running_binary_sha256().await,
            // The commit the running bytes were built from, so two builds
            // with the same version are told apart at a glance.
            "commit": crate::upgrade::version::build_commit(),
            "compatibility": crate::compatibility::CURRENT,
            "upgrade_in_flight": manager.upgrade_in_flight(),
            // Ids this node attempted and reverted — the orchestrator
            // reads these to detect node-side reverts.
            "failed_upgrade_ids": manager.reverted_upgrade_ids(),
            // The leader refuses a cluster upgrade up front when a node
            // reports false, rather than recording a run the node will
            // refuse and leaving it paused.
            "accepts_network_upgrades": manager.accepts_network_upgrades(),
            // What a rollback could return to. The leader refuses a
            // cluster rollback up front when a node lacks the target.
            "installed_versions": manager.installed_versions().await,
        }),
        None => serde_json::json!({
            "version": crate::upgrade::version::compiled_version().to_string(),
            "commit": crate::upgrade::version::build_commit(),
            "compatibility": crate::compatibility::CURRENT,
            "upgrade_in_flight": false,
            "failed_upgrade_ids": [],
            // No upgrade manager, so no way to apply a directive at all.
            "accepts_network_upgrades": false,
        }),
    }
}

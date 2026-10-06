//! Brioche web UI routes: login and session, the dashboard, detail pages
//! and htmx fragments.

use super::*;

/// Render the dashboard login page.
pub(super) async fn login_handler() -> Response {
    axum::response::Html(crate::brioche::login::render_login(None)).into_response()
}

/// Form body for the login/session exchange.
#[derive(Deserialize)]
pub(super) struct SessionForm {
    pub(super) token: String,
}

/// Exchange an API token for a read-only session cookie.
///
/// The browser posts a token once; on success it receives an `HttpOnly`,
/// `SameSite=Strict` cookie and is redirected to the dashboard. The session
/// is read-only regardless of the token's role.
pub(super) async fn ui_session_handler(
    State(auth): State<crate::sesame::auth::AuthState>,
    axum::Form(form): axum::Form<SessionForm>,
) -> Response {
    // Accept the internal service token or any valid user token. The session
    // inherits the presented token's scope (C3), so a tenant-scoped token
    // cannot widen to cluster-wide reads by exchanging itself for a cookie. It
    // also records which exact token it came from and that token's expiry
    // (B11), so the auth middleware can end it when the token is revoked or
    // lapses.
    let identity = if auth
        .service_token
        .as_deref()
        .is_some_and(|s| crate::sesame::auth::tokens_equal(&form.token, s))
    {
        // The operator presented the real service token; the session is
        // unconfined (but still read-only), matching the service principal.
        Some((
            crate::sesame::session::SessionIdentity {
                token_name: crate::sesame::auth::SYSTEM_PRINCIPAL.to_string(),
                principal_id: crate::sesame::auth::SYSTEM_PRINCIPAL.to_string(),
                scope: crate::sesame::types::TokenScope::default(),
            },
            None,
        ))
    } else {
        // Snapshot the tokens under the lock, then verify through the same
        // bounded path as a bearer (B12): a malformed token is refused by a
        // string check before any hashing, and a well-shaped one waits for a
        // permit from the process-wide Argon2 semaphore. This route is
        // unauthenticated, so calling Argon2 directly here would let anyone on
        // the network pin every core and exhaust memory with junk logins.
        let tokens = auth.tokens.read().await.clone();
        crate::sesame::auth::authenticate_off_lock(&form.token, tokens.clone())
            .await
            .ok()
            .map(|ctx| {
                // A session opened with a rotated-out secret lives no longer
                // than that secret's grace period.
                let expires_at = crate::sesame::auth::find_credential(&ctx.principal_id, &tokens)
                    .and_then(|credential| credential.valid_until);
                (
                    crate::sesame::session::SessionIdentity {
                        token_name: ctx.token_name,
                        principal_id: ctx.principal_id,
                        scope: crate::sesame::types::TokenScope {
                            apps: ctx.scoped_apps,
                            namespaces: ctx.scoped_namespaces,
                        },
                    },
                    expires_at,
                )
            })
    };

    let Some((identity, expires_at)) = identity else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::response::Html(crate::brioche::login::render_login(Some(
                "invalid or expired token",
            ))),
        )
            .into_response();
    };

    let session = auth.sessions.create(identity, expires_at).await;
    // The cookie lives no longer than the session, which lives no longer
    // than the token.
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        crate::sesame::session::SESSION_COOKIE,
        session.id,
        session.lifetime.as_secs()
    );
    (
        [(axum::http::header::SET_COOKIE, cookie)],
        axum::response::Redirect::to("/"),
    )
        .into_response()
}

/// Clear the current session (logout).
pub(super) async fn ui_logout_handler(
    State(auth): State<crate::sesame::auth::AuthState>,
    headers: HeaderMap,
) -> Response {
    if let Some(id) = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::sesame::session::session_id_from_cookie_header)
    {
        auth.sessions.remove(id).await;
    }
    let cleared = format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        crate::sesame::session::SESSION_COOKIE
    );
    (
        [(axum::http::header::SET_COOKIE, cleared)],
        axum::response::Redirect::to("/ui/login"),
    )
        .into_response()
}

/// Build dashboard app rows from instance statuses.
pub(super) fn statuses_to_dashboard_apps(
    statuses: &[InstanceStatus],
    desired: &[crate::bun::diagnostics::DesiredAppEvidence],
) -> Vec<DashboardApp> {
    let mut rows = std::collections::BTreeMap::new();
    for app in desired {
        rows.insert(
            (app.namespace.clone(), app.app.clone()),
            DashboardApp {
                name: app.app.clone(),
                namespace: app.namespace.clone(),
                instances_running: 0,
                instances_desired: app.desired_replicas as usize,
                state: "pending".to_string(),
            },
        );
    }
    for instance in statuses {
        let Some(row) = rows.get_mut(&(instance.namespace.clone(), instance.app_name.clone()))
        else {
            continue;
        };
        if instance.state == "running" {
            row.instances_running += 1;
        }
        if matches!(instance.state.as_str(), "failed" | "unhealthy") {
            row.state = "unhealthy".into();
        }
    }
    // A quota block, or a volume app waiting for its home node (#423): either
    // way the scheduler holds it back on purpose, so it reads as blocked.
    for app in desired
        .iter()
        .filter(|app| app.blocked.is_some() || app.volume_home_away.is_some())
    {
        if let Some(row) = rows.get_mut(&(app.namespace.clone(), app.app.clone()))
            && row.state == "pending"
        {
            row.state = "blocked".into();
        }
    }
    for row in rows.values_mut() {
        if row.state != "unhealthy" && row.instances_running == row.instances_desired {
            row.state = if row.instances_desired == 0 {
                "stopped"
            } else {
                "running"
            }
            .into();
        }
    }
    rows.into_values().collect()
}

pub(super) async fn gather_dashboard_apps(
    state: &ApiState,
    directory: Option<&LeaderDirectory>,
) -> Result<Vec<DashboardApp>, String> {
    let (statuses, desired) = tokio::try_join!(
        cluster_statuses(state),
        gather_desired_apps(state, DesiredAppsSource::Caller(directory))
    )?;
    let statuses: Vec<_> = statuses.into_iter().map(|row| row.instance).collect();
    Ok(statuses_to_dashboard_apps(&statuses, &desired))
}

/// Build the dashboard data from current agent state.
pub(super) async fn gather_dashboard_data(
    state: &ApiState,
    directory: Option<&LeaderDirectory>,
) -> Result<DashboardData, String> {
    let apps = gather_dashboard_apps(state, directory).await?;

    let alerts = firing_dashboard_alerts(state).await;
    let alert_count = alerts.len();

    let nodes = gather_dashboard_nodes(state).await;
    // The node count follows the real membership when we have it. A
    // standalone node with no gossip table still shows itself as one node.
    let node_count = if nodes.is_empty() { 1 } else { nodes.len() };

    Ok(DashboardData {
        cluster_name: String::new(),
        node_count,
        app_count: apps.len(),
        alert_count,
        apps,
        nodes,
        alerts,
    })
}

/// Build the dashboard node rows from the live gossip membership (AUTH7).
///
/// The membership table only holds nodes gossip currently considers alive, so
/// every row here is a live member. When the council is up we also count each
/// node's assigned apps from the desired state, giving the same per-node app
/// totals the placements endpoint serves. No membership table (standalone)
/// yields an empty list, and the caller falls back to a single-node view.
pub(super) async fn gather_dashboard_nodes(
    state: &ApiState,
) -> Vec<crate::brioche::dashboard::DashboardNode> {
    let Some(membership) = &state.membership else {
        return vec![];
    };
    let members = membership.read().await;

    // App counts per node, when we can see the desired state.
    let mut app_counts: std::collections::HashMap<crate::meat::NodeId, usize> =
        std::collections::HashMap::new();
    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for placements in desired.scheduling.values() {
            for placement in placements {
                *app_counts.entry(placement.node_id.clone()).or_insert(0) += 1;
            }
        }
    }

    members
        .iter()
        .map(|member| crate::brioche::dashboard::DashboardNode {
            name: member.node_id.0.clone(),
            state: "alive".to_string(),
            app_count: app_counts.get(&member.node_id).copied().unwrap_or(0),
        })
        .collect()
}

/// Return an HTML response.
pub(super) fn html_response(html: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

/// `GET /` — serve the Brioche cluster overview dashboard.
///
/// The alert panel follows `/v1/alerts`: a principal whose `[permission]`
/// spec doesn't grant `metrics` across the cluster sees the page without it.
pub(super) async fn dashboard_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    State(state): State<ApiState>,
) -> Response {
    let mut data = match gather_dashboard_data(&state, directory.as_deref()).await {
        Ok(data) => data,
        Err(error) => return unavailable_response(error),
    };
    if enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    .is_err()
    {
        data.alerts.clear();
        data.alert_count = 0;
    }
    html_response(render_dashboard(&data))
}

/// Names of the metrics an app's instances reported in the last five
/// minutes, for choosing its page's charts. Empty if the query fails or
/// takes more than three seconds: the page renders without those charts
/// rather than waiting on a slow node.
pub(super) async fn scraped_metric_names(
    state: &ApiState,
    app: &str,
    namespace: &str,
) -> Vec<String> {
    let start = crate::mayo::types::Sample::now(0.0)
        .timestamp
        .saturating_sub(300);
    let query = app_metric_rows(state, app, namespace, None, start, i64::MAX as u64, Some(1));
    let Ok(Ok(result)) = tokio::time::timeout(std::time::Duration::from_secs(3), query).await
    else {
        return Vec::new();
    };
    let names: std::collections::BTreeSet<String> =
        result.data.into_iter().map(|row| row.metric_name).collect();
    names.into_iter().collect()
}

/// `GET /ui/app/{app}/{namespace}` — app detail page.
pub(super) async fn app_detail_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let (rows, desired) = match tokio::try_join!(
        cluster_statuses(&state),
        gather_desired_apps(&state, DesiredAppsSource::Caller(directory.as_deref()))
    ) {
        Ok(result) => result,
        Err(error) => return unavailable_response(error),
    };
    let instances: Vec<InstanceStatus> = rows
        .into_iter()
        .map(|row| row.instance)
        .filter(|instance| instance.app_name == app && instance.namespace == namespace)
        .collect();
    let summary = statuses_to_dashboard_apps(&instances, &desired)
        .into_iter()
        .find(|row| row.name == app && row.namespace == namespace);
    let (overall_state, desired_instances) = summary
        .map(|row| (row.state, row.instances_desired))
        .unwrap_or_else(|| ("unknown".to_string(), 0));
    let blocked =
        desired
            .iter()
            .find(|evidence| evidence.app == app && evidence.namespace == namespace)
            .and_then(|evidence| {
                evidence
                    .blocked
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| {
                        evidence.volume_home_away.as_ref().map(|home| {
                    format!("waiting for {home}, which holds its volume and is out of the cluster")
                })
                    })
            });

    let env = if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        desired
            .apps
            .iter()
            .find(|(id, _)| id.name == app && id.namespace == namespace)
            .map(|(_, spec)| safe_env(&spec.env))
            .unwrap_or_default()
    } else {
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (response, receiver) = oneshot::channel();
            state
                .cmd_tx
                .send(AgentCommand::AppConfig {
                    app_name: app.clone(),
                    namespace: namespace.clone(),
                    response,
                })
                .await
                .map_err(|_| "agent unavailable")?;
            receiver
                .await
                .map_err(|_| "agent did not return app configuration")
        })
        .await;
        match result {
            Ok(Ok(Some(spec))) => safe_env(&spec.env),
            Ok(Ok(None)) => Vec::new(),
            Ok(Err(error)) => return unavailable_response(error.to_string()),
            Err(_) => return unavailable_response("app configuration query timed out".to_string()),
        }
    };

    let deploy_history = cluster_deploy_history(&state, &app, &namespace, false).await;

    // Each chart polls the app's metric endpoint, which refuses a principal
    // without `metrics` on this app; leave them out rather than draw errors.
    let charts = match enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
        Ok(()) => crate::brioche::app_detail::app_charts(
            &app,
            &namespace,
            &scraped_metric_names(&state, &app, &namespace).await,
        ),
        Err(_) => Vec::new(),
    };

    let data = AppDetailData {
        app_name: app,
        namespace,
        state: overall_state,
        blocked,
        desired_instances,
        instances,
        env,
        deploy_history: deploy_history.history,
        history_warnings: deploy_history.warnings,
        charts,
    };

    html_response(render_app_detail(&data))
}

/// `GET /ui/node/{name}` — node detail page.
///
/// Any node's page lists that node's workloads: this node answers from its
/// own status, another member is asked for its status directly, and a
/// member that doesn't answer is named on the page rather than shown empty.
pub(super) async fn node_detail_handler(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Response {
    let is_self = local_node_name(&state) == name;
    let member = match &state.membership {
        Some(m) => m
            .read()
            .await
            .iter()
            .find(|info| info.node_id.0 == name)
            .cloned(),
        None => None,
    };
    let node_state = if is_self || member.is_some() {
        "alive"
    } else {
        "unknown"
    }
    .to_string();
    let (statuses, warning) = match (is_self, &member) {
        (true, _) => (gather_statuses(&state).await, None),
        (false, Some(member)) => {
            match fetch_from_peer::<Vec<InstanceStatus>>(
                &state,
                member,
                "/v1/status",
                CLUSTER_STATUS_TIMEOUT,
            )
            .await
            {
                Ok(statuses) => (statuses, None),
                Err(error) => (Vec::new(), Some(format!("did not answer: {error}"))),
            }
        }
        (false, None) => (Vec::new(), None),
    };
    // The charts read this node's metrics store, so drawing them on another
    // node's page would label this node's CPU and memory as that node's.
    let charts = if is_self { node_charts() } else { Vec::new() };

    let data = NodeDetailData {
        name,
        state: node_state,
        app_count: statuses.len(),
        apps: statuses,
        warning,
        charts,
    };

    html_response(render_node_detail(&data))
}

/// CPU and memory charts for this node's own page.
pub(super) fn node_charts() -> Vec<ChartConfig> {
    vec![
        ChartConfig {
            endpoint: "/v1/metrics?name=node_cpu_usage_percent".to_string(),
            title: "CPU Usage".to_string(),
            unit: crate::brioche::units::ChartUnit::Percent,
            refresh_secs: 10,
            range_secs: 3600,
        },
        ChartConfig {
            endpoint: "/v1/metrics?name=node_memory_used_bytes".to_string(),
            title: "Memory Usage".to_string(),
            unit: crate::brioche::units::ChartUnit::Bytes,
            refresh_secs: 10,
            range_secs: 3600,
        },
    ]
}

/// `GET /ui/gitops` — Lettuce GitOps status page: current sync phase,
/// coordinator, last applied commit, and recent sync history (E).
pub(super) async fn gitops_handler(State(state): State<ApiState>) -> Response {
    let sync = match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .gitops_sync_state
            .unwrap_or_default(),
        None => crate::lettuce::types::SyncState::default(),
    };
    html_response(crate::brioche::gitops::render_gitops(&sync))
}

/// `GET /ui/fragment/apps` — apps table HTML fragment for HTMX swap.
pub(super) async fn fragment_apps_handler(
    directory: Option<axum::Extension<LeaderDirectory>>,
    State(state): State<ApiState>,
) -> Response {
    match gather_dashboard_apps(&state, directory.as_deref()).await {
        Ok(apps) => html_response(fragments::render_apps_table_fragment(&apps)),
        Err(error) => unavailable_response(error),
    }
}

/// `GET /ui/fragment/nodes` — nodes table HTML fragment for HTMX swap.
pub(super) async fn fragment_nodes_handler(State(state): State<ApiState>) -> Response {
    // AUTH7: reflect the real gossip membership, not a hardcoded empty list.
    let nodes = gather_dashboard_nodes(&state).await;
    html_response(fragments::render_nodes_table_fragment(&nodes))
}

/// `GET /ui/fragment/alerts` — alerts table HTML fragment for HTMX swap.
pub(super) async fn fragment_alerts_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let alerts = firing_dashboard_alerts(&state).await;
    html_response(fragments::render_alerts_table_fragment(&alerts))
}

/// The firing alerts, shaped for the dashboard's alert table.
pub(super) async fn firing_dashboard_alerts(
    state: &ApiState,
) -> Vec<crate::brioche::dashboard::DashboardAlert> {
    let Some(evaluator) = &state.alerts else {
        return Vec::new();
    };
    evaluator
        .read()
        .await
        .firing_alerts()
        .iter()
        .map(|a| crate::brioche::dashboard::DashboardAlert {
            labels: a.labels.clone(),
            name: a.rule_name.clone(),
            severity: format!("{:?}", a.severity),
            description: a.description.clone(),
        })
        .collect()
}

/// `GET /ui/fragment/app/{app}/{namespace}/instances` — instance table fragment.
pub(super) async fn fragment_instances_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    let statuses = match cluster_statuses(&state).await {
        Ok(rows) => rows.into_iter().map(|row| row.instance).collect::<Vec<_>>(),
        Err(error) => return unavailable_response(error),
    };
    let instances: Vec<InstanceStatus> = statuses
        .into_iter()
        .filter(|s| s.app_name == app && s.namespace == namespace)
        .collect();
    html_response(fragments::render_instance_table_fragment(&instances))
}

/// `GET /ui/app/{app}/{namespace}/env` — safe environment variables (JSON).
///
/// Encrypted values are replaced with `"[encrypted]"`. The raw
/// ciphertext never reaches the browser.
pub(super) async fn app_env_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    match ask_agent(&state.cmd_tx, |response| AgentCommand::AppConfig {
        app_name: app,
        namespace,
        response,
    })
    .await
    {
        Ok(Some(spec)) => Json(safe_env(&spec.env)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "app not found"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response(),
    }
}

/// A summary table refreshes independently of the app/instance tables.
pub(super) async fn fragment_batches_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let rows = crate::bun::task_array_api::summaries(&state, auth.as_deref()).await;
    html_response(crate::brioche::dashboard::render_batches(&rows))
}

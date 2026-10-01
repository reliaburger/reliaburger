//! Metrics and alert routes: local and cluster queries, rollups and the
//! dashboard's app charts.

use super::*;

#[derive(Deserialize)]
pub(super) struct MetricsQueryParams {
    pub(super) name: Option<String>,
    pub(super) start: Option<u64>,
    pub(super) end: Option<u64>,
    /// Restrict to one app's samples, matched against the `app` label
    /// (`namespace/app`). Set by the single-app cross-node fan-out so each
    /// node answers with only that app's local data; absent for node-wide
    /// dashboard queries.
    pub(super) app: Option<String>,
    /// Keep only the newest N samples of each series (per-app queries).
    pub(super) per_series: Option<u32>,
}

/// Window the per-app endpoint reads when the caller gives no `start`.
///
/// Callers want "what's happening now"; reading from the epoch made every
/// unbounded query scan (and cap) the whole retention period.
pub(super) const APP_METRICS_DEFAULT_WINDOW_SECS: u64 = 15 * 60;

/// `GET /v1/metrics?name=X&start=S&end=E` — query time-series data.
///
/// Reads across every app and namespace on the node, so a scoped token is
/// refused (C3) and pointed at `/v1/metrics/app/{app}/{namespace}`, which can
/// filter. The cross-node fan-out presents the service token, which passes.
pub(super) async fn metrics_query_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!({"error": "metrics not enabled"})).into_response();
    };

    let store = mayo.read().await;
    let name = params.name.as_deref().unwrap_or("*");
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    // When an `app` filter is present this is a leaf of the single-app
    // cross-node fan-out: answer with only that app's local samples. Every
    // caller-supplied string reaches the SQL literal, so escape each (OBS1).
    if let Some(app) = &params.app {
        let name = (name != "*").then_some(name);
        return match store
            .query_app(app, name, start, end, params.per_series)
            .await
        {
            Ok(results) => {
                let data: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(ts, name, labels, val)| {
                        serde_json::json!({"timestamp": ts, "metric_name": name, "labels": labels, "value": val})
                    })
                    .collect();
                Json(data).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        };
    }

    if name == "*" {
        match store.query_all(start, end).await {
            Ok(results) => {
                let data: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(ts, name, labels, val)| {
                        serde_json::json!({"timestamp": ts, "metric_name": name, "labels": labels, "value": val})
                    })
                    .collect();
                Json(data).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    } else {
        match store.query(name, start, end).await {
            Ok(results) => {
                let data: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(ts, name, labels, val)| {
                        serde_json::json!({"timestamp": ts, "metric_name": name, "labels": labels, "value": val})
                    })
                    .collect();
                Json(data).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
}

/// `GET /v1/metrics/summary` — latest value for each metric.
pub(super) async fn metrics_summary_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!([])).into_response();
    };

    let store = mayo.read().await;
    match store.metric_names().await {
        Ok(names) => {
            // Return the list of known metrics (full summary requires more complex SQL)
            Json(serde_json::json!({"metrics": names})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/alerts` — list all alert statuses.
///
/// Alerts are rules evaluated over the whole metric store, so a principal
/// with a `[permission]` spec needs `metrics` across the cluster (B18).
pub(super) async fn alerts_handler(
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
    let Some(alerts) = &state.alerts else {
        return Json(crate::mayo::alert::AlertsResponse { alerts: Vec::new() }).into_response();
    };
    let evaluator = alerts.read().await;
    Json(crate::mayo::alert::AlertsResponse {
        alerts: evaluator.all_statuses(),
    })
    .into_response()
}

/// `GET /v1/metrics/keys` — list all distinct metric names.
pub(super) async fn metrics_keys_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let Some(mayo) = &state.mayo else {
        return Json(serde_json::json!({"keys": []})).into_response();
    };

    let store = mayo.read().await;
    match store.metric_names().await {
        Ok(names) => Json(serde_json::json!({"keys": names})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/metrics/rollup?name=X&start=S&end=E` — query local rollup store.
///
/// Internal endpoint used by cluster-wide query fan-out. Each council
/// member evaluates this against its own rollup data.
pub(super) async fn metrics_rollup_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let Some(rollup_store) = &state.rollup_store else {
        return Json(Vec::<MetricsQueryRow>::new()).into_response();
    };

    let store = rollup_store.read().await;
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    let result = match &params.name {
        Some(name) => store.query_cluster_metric(name, start, end).await,
        None => store.query_all(start, end).await,
    };

    match result {
        Ok(rows) => {
            let data: Vec<MetricsQueryRow> = rows
                .into_iter()
                .map(|(ts, name, labels, val)| MetricsQueryRow {
                    timestamp: ts,
                    metric_name: name,
                    labels,
                    value: val,
                })
                .collect();
            Json(data).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Return worker identity with each contribution so overlapping aggregators cannot double-count it.
pub(super) async fn metrics_owned_rollup_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    if let Err(response) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return response;
    }
    let Some(store) = &state.rollup_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no rollup store configured",
        )
            .into_response();
    };
    match store
        .read()
        .await
        .query_owned_rows(
            params.name.as_deref(),
            params.start.unwrap_or(0),
            params.end.unwrap_or(i64::MAX as u64),
        )
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

/// Resolve the base URLs of every council aggregator (Raft voter) from the
/// live gossip membership.
///
/// Each aggregator holds only its assigned workers' rollups, so a cluster-wide
/// query must reach all of them and sum the partial aggregates. Returns `None`
/// when this node has no council or no membership table (the standalone case),
/// or when no voter can be mapped to a live member — the caller then reads its
/// own local rollup store instead. This node's own URL is included when it is a
/// voter, so a single-node council fans out to just itself.
pub(super) async fn resolve_council_urls(state: &ApiState) -> Option<Vec<String>> {
    let council = state.council.as_ref()?;
    let membership = state.membership.as_ref()?;
    let metrics = council.metrics().borrow().clone();
    let voters: std::collections::BTreeSet<_> =
        metrics.membership_config.membership().voter_ids().collect();
    if voters.is_empty() {
        return None;
    }
    let members = membership.read().await;
    let urls: Vec<String> = members
        .iter()
        .filter(|member| {
            voters.contains(&crate::cluster::identity::raft_id_from_name(
                &member.node_id.0,
            ))
        })
        .map(|member| state.cluster_http.url(&member.address.to_string(), ""))
        .collect();
    if urls.is_empty() { None } else { Some(urls) }
}

/// `GET /v1/metrics/cluster?name=X&start=S&end=E` — cluster-wide query.
///
/// Fans out to all council aggregators' `/v1/metrics/rollup/owned` endpoints,
/// deduplicates worker contributions before summing, and returns the combined data
/// with any warnings about unresponsive aggregators. Falls back to reading the
/// local rollup store when there is no council to fan out to (single-node).
pub(super) async fn metrics_cluster_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return resp;
    }
    if let Err(resp) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
    )
    .await
    {
        return resp;
    }
    let start = params.start.unwrap_or(0);
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = params.end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);

    // Retain worker identity until after deduplication: reassignment leaves
    // overlapping history on old and new aggregators.
    if let Some(urls) = resolve_council_urls(&state).await {
        let query = MetricsQuery {
            metric_name: params.name.clone(),
            start,
            end,
            app: None,
            per_series: None,
        };
        let timeout = std::time::Duration::from_secs(10);
        let result = crate::mayo::query_fanout::fan_out_cluster_query(
            &query,
            &urls,
            state.cluster_http.client(),
            timeout,
            state.service_token.as_deref(),
        )
        .await;
        return Json(result).into_response();
    }

    // Single-node / no-council fallback: read the local rollup store directly,
    // which is equivalent to fanning out to just ourselves.
    let Some(rollup_store) = &state.rollup_store else {
        return Json(MetricsQueryResult {
            data: vec![],
            warnings: vec![QueryWarning::NodeUnresponsive {
                node_id: "no rollup store configured".to_string(),
            }],
        })
        .into_response();
    };

    let store = rollup_store.read().await;
    let result = store
        .query_owned_rows(params.name.as_deref(), start, end)
        .await;
    match result {
        Ok(rows) => {
            Json(crate::mayo::query_fanout::merge_owned_rollups(vec![rows])).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// One app's metric rows, wherever its instances run.
///
/// When the placement map is visible (council + membership), fans out to the
/// nodes running the app, hitting each one's app-filtered `/v1/metrics` leaf
/// and merge-sorting the per-instance rows. Falls back to the local metrics
/// store otherwise (single-node, or no placement info) — which is the same as
/// fanning out to just this node. `Err` carries a store failure message.
pub(super) async fn app_metric_rows(
    state: &ApiState,
    app: &str,
    namespace: &str,
    name: Option<&str>,
    start: u64,
    end: u64,
    per_series: Option<u32>,
) -> Result<MetricsQueryResult, String> {
    // Cross-node fan-out: each node keeps only its own instances' samples, so
    // reading just this node's store misses instances scheduled elsewhere.
    if let (Some(council), Some(membership)) = (&state.council, &state.membership) {
        use crate::meat::types::AppId;
        let desired = council.desired_state().await;
        let app_id = AppId::new(app, namespace);
        let node_ids: Vec<crate::meat::NodeId> = desired
            .scheduling
            .get(&app_id)
            .map(|placements| placements.iter().map(|p| p.node_id.clone()).collect())
            .unwrap_or_default();

        if !node_ids.is_empty() {
            let members = membership.read().await;
            let urls: Vec<String> = node_ids
                .iter()
                .filter_map(|id| members.iter().find(|m| m.node_id == *id))
                .map(|m| state.cluster_http.url(&m.address.to_string(), ""))
                .collect();
            drop(members);

            if !urls.is_empty() {
                let query = MetricsQuery {
                    metric_name: name.map(str::to_string),
                    start,
                    end,
                    // The leaf filters on the `app` label, stored as `namespace/app`.
                    app: Some(format!("{namespace}/{app}")),
                    per_series,
                };
                let timeout = std::time::Duration::from_secs(10);
                return Ok(crate::mayo::query_fanout::fan_out_app_query(
                    &query,
                    &urls,
                    state.cluster_http.client(),
                    timeout,
                    state.service_token.as_deref(),
                )
                .await);
            }
        }
    }

    let Some(mayo) = &state.mayo else {
        return Ok(MetricsQueryResult {
            data: vec![],
            warnings: vec![],
        });
    };

    // Filter by app label in the local store. Both the app/namespace path
    // segments and the caller-supplied `name` reach the SQL literal, which
    // `query_app` escapes (OBS1): without that a crafted `?name=x' OR '1'='1`
    // or an app name carrying a quote would break out of the literal and drop
    // the tenant/time predicate, leaking other apps' metrics.
    let rows = mayo
        .read()
        .await
        .query_app(&format!("{namespace}/{app}"), name, start, end, per_series)
        .await
        .map_err(|error| error.to_string())?;
    Ok(MetricsQueryResult {
        data: rows
            .into_iter()
            .map(|(timestamp, metric_name, labels, value)| MetricsQueryRow {
                timestamp,
                metric_name,
                labels,
                value,
            })
            .collect(),
        warnings: vec![],
    })
}

/// The query window a per-app request names: `start` defaults to fifteen
/// minutes ago, `end` to now.
pub(super) fn app_query_window(start: Option<u64>, end: Option<u64>) -> (u64, u64) {
    let start = start.unwrap_or_else(|| {
        crate::mayo::types::Sample::now(0.0)
            .timestamp
            .saturating_sub(APP_METRICS_DEFAULT_WINDOW_SECS)
    });
    // Clamped below u64::MAX: DataFusion 45's interval analysis
    // overflows (debug-build panic) computing the cardinality of a
    // full-domain unsigned range like `timestamp <= u64::MAX`.
    let end = end.unwrap_or(i64::MAX as u64).min(i64::MAX as u64);
    (start, end)
}

pub(super) fn metrics_error_response(error: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// `GET /v1/metrics/app/{app}/{namespace}?name=X&start=S&end=E&per_series=N`
/// — one app's raw metric rows, across every node running it.
pub(super) async fn metrics_app_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<MetricsQueryParams>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let (start, end) = app_query_window(params.start, params.end);
    match app_metric_rows(
        &state,
        &app,
        &namespace,
        params.name.as_deref(),
        start,
        end,
        params.per_series,
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => metrics_error_response(error),
    }
}

#[derive(Deserialize)]
pub(super) struct AppChartParams {
    /// Metric to draw; a histogram's base name for `kind=mean`.
    pub(super) name: String,
    /// How rows become lines.
    pub(super) kind: crate::mayo::series::ChartKind,
    pub(super) start: Option<u64>,
    pub(super) end: Option<u64>,
}

/// What the dashboard's chart script draws: series lined up on one time
/// axis, plus any fan-out warnings.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AppChartResponse {
    #[serde(flatten)]
    pub(super) chart: crate::mayo::series::ChartData,
    pub(super) warnings: Vec<crate::mayo::rollup::QueryWarning>,
}

/// `GET /v1/metrics/app/{app}/{namespace}/chart?name=X&kind=gauge|rate|mean`
/// — one metric as one line per instance, ready to draw.
///
/// `gauge` draws values, `rate` draws a counter's per-second rate, and
/// `mean` draws `rate(X_sum) / rate(X_count)`, a histogram's mean.
pub(super) async fn metrics_app_chart_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
    Query(params): Query<AppChartParams>,
) -> Response {
    use crate::mayo::series::{self, ChartKind};

    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Metrics,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    let (start, end) = app_query_window(params.start, params.end);
    let fetch = |name: String| {
        let state = &state;
        let app = &app;
        let namespace = &namespace;
        async move { app_metric_rows(state, app, namespace, Some(&name), start, end, None).await }
    };
    let response = match params.kind {
        ChartKind::Gauge | ChartKind::Rate => {
            fetch(params.name.clone())
                .await
                .map(|result| AppChartResponse {
                    chart: series::instance_chart(params.kind, &result.data),
                    warnings: result.warnings,
                })
        }
        ChartKind::Mean => {
            match tokio::try_join!(
                fetch(format!("{}_sum", params.name)),
                fetch(format!("{}_count", params.name))
            ) {
                Ok((sum, count)) => {
                    let mut warnings = sum.warnings;
                    warnings.extend(count.warnings);
                    Ok(AppChartResponse {
                        chart: series::mean_chart(&sum.data, &count.data),
                        warnings,
                    })
                }
                Err(error) => Err(error),
            }
        }
    };
    match response {
        Ok(response) => Json(response).into_response(),
        Err(error) => metrics_error_response(error),
    }
}

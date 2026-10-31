//! The audit trail's API side: the layer that records every audited route,
//! and `GET /v1/audit`, which reads the durable log back (#685).
//!
//! Most audited actions are recorded here, by one layer, rather than by
//! each handler: the layer knows the route, the caller and the outcome, so
//! a new handler can't forget to audit. [`AUDITED_ROUTES`] lists the routes
//! it records. A handler that knows more than its path (the namespace in a
//! manifest, the node being decommissioned) adds it with
//! [`crate::bun::audit::annotate`]. Token, CA, secret and fault routes
//! record their own, richer records ([`HANDLER_AUDITED_ROUTES`]).

use super::*;

use crate::bun::audit::{AuditFilter, AuditListing, AuditNote, AuditRecord};
use crate::bun::authz::Method as RouteMethod;
use crate::bun::events::EventKind;

/// A route the audit layer records when it succeeds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuditedRoute {
    pub(crate) method: RouteMethod,
    /// The axum path pattern exactly as mounted.
    pub(crate) path: &'static str,
    /// The stable action name the record carries.
    pub(crate) action: &'static str,
    pub(crate) kind: EventKind,
    /// The path parameter that names the app, when it isn't `{app}`.
    pub(crate) app_parameter: &'static str,
}

const fn audited(
    method: RouteMethod,
    path: &'static str,
    action: &'static str,
    kind: EventKind,
) -> AuditedRoute {
    AuditedRoute {
        method,
        path,
        action,
        kind,
        app_parameter: "app",
    }
}

use EventKind::{Identity, Operation};
use RouteMethod::{Delete, Get, Post};

/// Every route the audit layer records, with the action it records.
pub(crate) const AUDITED_ROUTES: &[AuditedRoute] = &[
    audited(Post, "/v1/apply", "app.applied", Operation),
    audited(Post, "/v1/stop/{app}/{namespace}", "app.stopped", Operation),
    audited(
        Post,
        "/v1/delete/{app}/{namespace}",
        "app.deleted",
        Operation,
    ),
    audited(
        Post,
        "/v1/release/{app}/{namespace}",
        "app.released",
        Operation,
    ),
    audited(Post, "/v1/exec/{app}/{namespace}", "app.exec", Operation),
    audited(
        Post,
        "/v1/rollback/{app}/{namespace}",
        "deploy.rolled_back",
        Operation,
    ),
    audited(
        Post,
        "/v1/deploys/operations/{id}/cancel",
        "deploy.cancelled",
        Operation,
    ),
    audited(Post, "/v1/jobs/runs", "job.submitted", Operation),
    audited(Post, "/v1/jobs/runs/{id}/replay", "job.replayed", Operation),
    AuditedRoute {
        app_parameter: "name",
        ..audited(
            Post,
            "/v1/jobs/definitions/{name}/{namespace}/disable",
            "job.schedule_disabled",
            Operation,
        )
    },
    audited(Post, "/v1/batch", "batch.submitted", Operation),
    audited(Post, "/v1/batch/array", "array.submitted", Operation),
    audited(Post, "/v1/batch/manifest", "array.submitted", Operation),
    audited(Post, "/v1/batch/{id}/cancel", "batch.cancelled", Operation),
    audited(Post, "/v1/build", "build.submitted", Operation),
    audited(Post, "/v1/test/leases", "test_lease.created", Operation),
    audited(
        Delete,
        "/v1/test/leases/{id}",
        "test_lease.released",
        Operation,
    ),
    audited(
        Post,
        "/v1/test/leases/retired",
        "test_lease.retired",
        Operation,
    ),
    audited(Post, "/v1/rollouts/failed", "rollout.failed", Operation),
    audited(Post, "/v1/upgrade/apply", "upgrade.applied", Operation),
    audited(
        Post,
        "/v1/upgrade/rollback",
        "upgrade.rolled_back",
        Operation,
    ),
    audited(Post, "/v1/upgrade/start", "upgrade.started", Operation),
    audited(Post, "/v1/upgrade/resume", "upgrade.resumed", Operation),
    audited(Post, "/v1/upgrade/abort", "upgrade.aborted", Operation),
    audited(
        Post,
        "/v1/upgrade/cluster-rollback",
        "upgrade.cluster_rolled_back",
        Operation,
    ),
    audited(Post, "/v1/cluster/elect", "council.elect", Operation),
    audited(Post, "/v1/gitops/config", "gitops.configured", Operation),
    audited(
        Post,
        "/v1/policies/{name}/delete",
        "policy.deleted",
        Operation,
    ),
    audited(
        Post,
        "/v1/nodes/decommission",
        "node.decommissioned",
        Operation,
    ),
    audited(
        Post,
        "/v1/snapshots/{namespace}/{app}",
        "snapshot.created",
        Operation,
    ),
    audited(
        Post,
        "/v1/snapshots/{namespace}/{app}/restore",
        "snapshot.restored",
        Operation,
    ),
    audited(
        Delete,
        "/v1/snapshots/{namespace}/{app}/{name}",
        "snapshot.deleted",
        Operation,
    ),
    audited(Post, "/v1/logs/export", "logs.exported", Operation),
    audited(Post, "/v1/identity/sign", "identity.signed", Identity),
];

/// Mutating routes whose handlers record their own audit events, with the
/// fault id, token name or rotation stage only they know.
pub(crate) const HANDLER_AUDITED_ROUTES: &[(RouteMethod, &str)] = &[
    (Post, "/v1/fault"),
    (Delete, "/v1/fault"),
    (Delete, "/v1/fault/{id}"),
    (Post, "/v1/token/create"),
    (Post, "/v1/token/revoke"),
    (Post, "/v1/token/rotate"),
    (Post, "/v1/join-token/create"),
    (Post, "/v1/secret/rotate"),
    (Post, "/v1/ca/rotation/prepare"),
    (Post, "/v1/ca/rotation/begin"),
    (Post, "/v1/ca/rotation/finalize"),
];

/// Mutating routes that are not audited, and why. A new `POST` or `DELETE`
/// route must go in one of the three lists, or a test fails.
#[cfg(test)]
pub(crate) const UNAUDITED_ROUTES: &[(RouteMethod, &str, &str)] = &[
    (
        Post,
        "/v1/cluster/join",
        "public: the join token is the credential, and the council logs the join",
    ),
    (Post, "/ui/session", "a login, not a change to the cluster"),
    (Post, "/ui/logout", "a logout, not a change to the cluster"),
    (
        Post,
        "/v1/gitops/webhook",
        "public and signed by the git provider; the sync it triggers applies as the cluster",
    ),
    (Post, "/v1/path", "a read: it traces a request path"),
    (
        Post,
        "/v1/test/leases/{id}/renew",
        "a heartbeat on a lease whose creation is audited",
    ),
    (
        Post,
        "/v1/nodes/{node}/relay/{*path}",
        "the relayed request is audited by the node that serves it",
    ),
    (
        Post,
        "/v1/cluster/renew",
        "node-to-node certificate renewal",
    ),
    (
        Post,
        "/v1/cluster/trust-ack",
        "node-to-node trust acknowledgement",
    ),
    (
        Post,
        "/v1/cluster/workload-csr",
        "node-to-node certificate signing",
    ),
    (
        Post,
        "/v1/registry/propose",
        "node-to-node registry replication",
    ),
    (Post, "/v1/registry/query", "node-to-node registry read"),
    (
        Post,
        "/v1/chaos/reserve",
        "node-to-node half of an audited fault injection",
    ),
    (
        Post,
        "/v1/chaos/fence",
        "node-to-node half of an audited fault clear",
    ),
    (
        Post,
        "/v1/discovery/retire",
        "node-to-node service discovery",
    ),
    (
        Post,
        "/v1/discovery/withdrawn",
        "node-to-node service discovery",
    ),
    (
        Post,
        "/v1/batch/run",
        "node-to-node dispatch of an audited batch",
    ),
    (
        Post,
        "/v1/batch/{id}/report",
        "node-to-node progress report",
    ),
    (
        Post,
        "/v1/batch/array/sync",
        "node-to-node array grant sync, many times a second",
    ),
    (
        Post,
        "/v1/build/run",
        "node-to-node dispatch of an audited build",
    ),
    (Post, "/v1/build/track", "node-to-node build tracking"),
    (
        Post,
        "/v1/build/sign",
        "node-to-node signing of an audited build",
    ),
];

/// The audited route `method` and the matched `path` pattern name, if any.
pub(crate) fn audited_route(
    method: &axum::http::Method,
    path: &str,
) -> Option<&'static AuditedRoute> {
    let method = match *method {
        axum::http::Method::GET => Get,
        axum::http::Method::POST => Post,
        axum::http::Method::DELETE => Delete,
        _ => return None,
    };
    AUDITED_ROUTES
        .iter()
        .find(|route| route.method == method && route.path == path)
}

/// The layer around every authenticated route. It makes the caller's
/// address available to audit code inside the handler, and records each
/// [`AUDITED_ROUTES`] request that succeeds, synced to disk before the
/// response leaves.
///
/// A refused request isn't recorded here: a request with no valid
/// credential never gets this far. Instead this layer marks a refusal with
/// who the caller was ([`RefusedCaller`]), and [`refusal_layer`], outside
/// the auth layer, records every refusal.
pub(super) async fn audit_layer(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let caller = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0);
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .and_then(|matched| audited_route(request.method(), matched.as_str()));
    let auth = request
        .extensions()
        .get::<crate::sesame::auth::AuthContext>()
        .cloned();
    let Some(route) = route else {
        let (mut response, note) =
            crate::bun::audit::scope_request(caller, next.run(request)).await;
        mark_refusal(&mut response, auth.as_ref(), note);
        return response;
    };
    let (mut parts, body) = request.into_parts();
    let parameters = path_parameters(&mut parts).await;
    let request = axum::extract::Request::from_parts(parts, body);
    let (mut response, note) = crate::bun::audit::scope_request(caller, next.run(request)).await;
    if response.status().is_success() {
        let record = record_route(&state, auth.as_ref(), route, parameters, note);
        let _ = crate::bun::audit::scope_request(caller, record).await;
    } else {
        mark_refusal(&mut response, auth.as_ref(), note);
    }
    response
}

/// The request's path parameters, in order. Empty when it has none.
async fn path_parameters(parts: &mut axum::http::request::Parts) -> Vec<(String, String)> {
    use axum::extract::FromRequestParts;

    match axum::extract::RawPathParams::from_request_parts(parts, &()).await {
        Ok(raw) => raw
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Who an authenticated, refused request came from, and what the handler
/// had learnt of its target before refusing. The audit layer attaches it to
/// a 401 or 403 response for [`refusal_layer`]; it never leaves the server.
#[derive(Debug, Clone)]
struct RefusedCaller {
    principal: String,
    token_name: String,
    app: Option<String>,
    namespace: Option<String>,
}

/// Attach a [`RefusedCaller`] to `response` if it refuses the request.
fn mark_refusal(
    response: &mut Response,
    auth: Option<&crate::sesame::auth::AuthContext>,
    note: AuditNote,
) {
    if !matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return;
    }
    let (principal, token_name) = audit_principal(auth);
    response.extensions_mut().insert(RefusedCaller {
        principal,
        token_name,
        app: note.app,
        namespace: note.namespace,
    });
}

/// A route whose refusals are recorded: every route whose success is.
struct RefusedRoute {
    /// The action a success would have recorded, when the layer records it.
    attempted: Option<&'static str>,
    /// The path parameter that names the app.
    app_parameter: &'static str,
}

fn refused_route(method: &axum::http::Method, path: &str) -> Option<RefusedRoute> {
    if let Some(route) = audited_route(method, path) {
        return Some(RefusedRoute {
            attempted: Some(route.action),
            app_parameter: route.app_parameter,
        });
    }
    let method = match *method {
        axum::http::Method::POST => Post,
        axum::http::Method::DELETE => Delete,
        _ => return None,
    };
    HANDLER_AUDITED_ROUTES
        .iter()
        .any(|(audited_method, audited_path)| *audited_method == method && *audited_path == path)
        .then_some(RefusedRoute {
            attempted: None,
            app_parameter: "app",
        })
}

/// The layer outside the auth layer that records refused requests to
/// audited routes (#798): a missing or unknown credential (401), or a role,
/// `[permission]` or scope refusal (403).
///
/// Each record names the principal (or `anonymous`), the source address,
/// the route, the app and namespace when known, and the reason. It never
/// holds the credential or the request body. A [`RefusalLimiter`] keeps a
/// flood from filling the bounded log: past a few refusals a minute from one
/// source, they are counted, and one summary record says how many.
///
/// [`RefusalLimiter`]: crate::bun::audit::refusals::RefusalLimiter
pub(super) async fn refusal_layer(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .and_then(|matched| {
            refused_route(request.method(), matched.as_str())
                .map(|route| (route, format!("{} {}", request.method(), matched.as_str())))
        });
    let Some((route, route_name)) = route else {
        return next.run(request).await;
    };
    let caller = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0);
    let (mut parts, body) = request.into_parts();
    let parameters = path_parameters(&mut parts).await;
    let mut response = next
        .run(axum::extract::Request::from_parts(parts, body))
        .await;
    use crate::sesame::auth::RefusalReason;
    let reason = match response.status() {
        StatusCode::UNAUTHORIZED => RefusalReason::Unauthenticated,
        StatusCode::FORBIDDEN => response
            .extensions()
            .get::<RefusalReason>()
            .copied()
            .unwrap_or(RefusalReason::Forbidden),
        _ => return response,
    };
    let refused = response.extensions_mut().remove::<RefusedCaller>();
    record_refusal(
        &state,
        Refusal {
            route,
            route_name,
            caller,
            parameters,
            reason,
            refused,
        },
    )
    .await;
    response
}

/// One refused request, as [`refusal_layer`] saw it.
struct Refusal {
    route: RefusedRoute,
    route_name: String,
    caller: Option<std::net::SocketAddr>,
    parameters: Vec<(String, String)>,
    reason: crate::sesame::auth::RefusalReason,
    refused: Option<RefusedCaller>,
}

/// The principal a refusal by a caller with no valid credential names.
const ANONYMOUS: &str = "anonymous";

/// Record `refusal` if the limiter lets it through, and any summaries of
/// suppressed refusals that are due.
async fn record_refusal(state: &ApiState, refusal: Refusal) {
    if state.events.is_none() {
        return;
    }
    let Refusal {
        route,
        route_name,
        caller,
        parameters,
        reason,
        refused,
    } = refusal;
    let mut app = None;
    let mut namespace = None;
    for (key, value) in parameters {
        if key == route.app_parameter {
            app = Some(value);
        } else if key == "namespace" {
            namespace = Some(value);
        }
    }
    let mut details = std::collections::BTreeMap::new();
    details.insert("route".to_string(), route_name.clone());
    details.insert("reason".to_string(), reason.as_str().to_string());
    if let Some(attempted) = route.attempted {
        details.insert("attempted".to_string(), attempted.to_string());
    }
    let principal = match refused {
        Some(refused) => {
            app = refused.app.or(app);
            namespace = refused.namespace.or(namespace);
            details.insert("token_name".to_string(), refused.token_name);
            refused.principal
        }
        None => ANONYMOUS.to_string(),
    };
    let now = unix_now_seconds();
    let verdict = state.refusals.lock().await.admit(
        crate::bun::audit::refusals::RefusalSource::Caller {
            address: caller.map(|address| address.ip()),
            principal: principal.clone(),
        },
        now,
    );
    write_suppressed_refusals(state, verdict.summaries, now).await;
    if !verdict.record {
        return;
    }
    let from = caller.map_or_else(String::new, |address| format!(" from {}", address.ip()));
    let message = format!(
        "refused {route_name} for {principal}{from}: {}",
        reason.as_str()
    );
    write_audit(
        state,
        crate::bun::events::AuditEvent {
            timestamp: now,
            kind: EventKind::Refusal,
            severity: crate::bun::events::EventSeverity::Warning,
            action: "api.refused".to_string(),
            principal,
            app,
            namespace,
            node: Some(local_node_name(state)),
            source_address: caller,
            details,
            message,
        },
    )
    .await;
}

/// The audit event that says how many refusals from one source a closed
/// window suppressed.
pub(super) fn suppressed_refusals_event(
    summary: crate::bun::audit::refusals::SuppressedRefusals,
    node: String,
    now: u64,
) -> crate::bun::events::AuditEvent {
    use crate::bun::audit::refusals::{REFUSAL_WINDOW_SECONDS, RefusalSource};
    let mut details = std::collections::BTreeMap::new();
    details.insert("suppressed".to_string(), summary.suppressed.to_string());
    details.insert("window_start".to_string(), summary.window_start.to_string());
    details.insert(
        "window_seconds".to_string(),
        REFUSAL_WINDOW_SECONDS.to_string(),
    );
    let (principal, from) = match summary.source {
        RefusalSource::Caller { address, principal } => {
            let from = match address {
                Some(address) => {
                    details.insert("source".to_string(), address.to_string());
                    format!("{principal} from {address}")
                }
                None => principal.clone(),
            };
            (principal, from)
        }
        RefusalSource::Overflow => ("*".to_string(), "other sources".to_string()),
    };
    crate::bun::events::AuditEvent {
        timestamp: now,
        kind: EventKind::Refusal,
        severity: crate::bun::events::EventSeverity::Warning,
        action: "api.refusals_suppressed".to_string(),
        principal,
        app: None,
        namespace: None,
        node: Some(node),
        source_address: None,
        details,
        message: format!(
            "{} more refused requests by {from} suppressed",
            summary.suppressed
        ),
    }
}

/// Record a summary for each window in `summaries`.
async fn write_suppressed_refusals(
    state: &ApiState,
    summaries: Vec<crate::bun::audit::refusals::SuppressedRefusals>,
    now: u64,
) {
    for summary in summaries {
        let event = suppressed_refusals_event(summary, local_node_name(state), now);
        write_audit(state, event).await;
    }
}

/// How often the summaries of closed refusal windows are written when no
/// new refusal arrives to carry them.
const REFUSAL_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Write the summary of every closed refusal window, every
/// [`REFUSAL_SWEEP_INTERVAL`], until the agent's command channel closes.
/// Without it a burst that stops would never be summarised.
pub(super) fn spawn_refusal_sweeper(state: ApiState) {
    if state.events.is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REFUSAL_SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = state.cmd_tx.closed() => return, _ = interval.tick() => {} }
            let now = unix_now_seconds();
            let summaries = state.refusals.lock().await.sweep(now);
            write_suppressed_refusals(&state, summaries, now).await;
        }
    });
}

/// Write the record for one successful audited request.
async fn record_route(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    route: &AuditedRoute,
    parameters: Vec<(String, String)>,
    note: AuditNote,
) {
    let mut app = None;
    let mut namespace = None;
    let mut details = std::collections::BTreeMap::new();
    for (key, value) in parameters {
        if key == route.app_parameter {
            app = Some(value);
        } else if key == "namespace" {
            namespace = Some(value);
        } else {
            details.insert(key, value);
        }
    }
    app = note.app.or(app);
    namespace = note.namespace.or(namespace);
    details.extend(note.details);
    let (_, token_name) = audit_principal(auth);
    let target = match (&app, &namespace) {
        (Some(app), Some(namespace)) => format!(" {app} in {namespace}"),
        (Some(app), None) => format!(" {app}"),
        (None, Some(namespace)) => format!(" in {namespace}"),
        (None, None) => String::new(),
    };
    let message = format!("{}{target} by {token_name}", route.action);
    record_caller_action(
        state,
        auth,
        CallerAction {
            kind: route.kind,
            severity: crate::bun::events::EventSeverity::Info,
            action: route.action,
            app,
            namespace,
            node: None,
            details,
            message,
        },
    )
    .await;
}

/// The audit note for an applied manifest: its namespace when it names one,
/// and the workloads it declares.
pub(super) fn manifest_audit_note(config: &Config) -> AuditNote {
    let mut workloads: Vec<(String, String)> = config
        .app
        .iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                spec.namespace.clone().unwrap_or_else(|| "default".into()),
            )
        })
        .chain(config.job.iter().map(|(name, spec)| {
            (
                name.clone(),
                spec.namespace.clone().unwrap_or_else(|| "default".into()),
            )
        }))
        .collect();
    workloads.sort();
    let namespaces: std::collections::BTreeSet<&str> = workloads
        .iter()
        .map(|(_, namespace)| namespace.as_str())
        .chain(config.namespace.keys().map(String::as_str))
        .collect();
    let mut details = std::collections::BTreeMap::new();
    if !workloads.is_empty() {
        details.insert("workloads".to_string(), summarise_names(&workloads));
    }
    if !namespaces.is_empty() {
        details.insert(
            "namespaces".to_string(),
            namespaces.iter().copied().collect::<Vec<_>>().join(","),
        );
    }
    // Every policy write is audited (#1117): the apply's record names them.
    if !config.policy.is_empty() {
        details.insert(
            "policies".to_string(),
            config.policy.keys().cloned().collect::<Vec<_>>().join(","),
        );
    }
    let single = |items: &[(String, String)]| match items {
        [(name, _)] => Some(name.clone()),
        _ => None,
    };
    AuditNote {
        app: single(&workloads),
        namespace: (namespaces.len() == 1)
            .then(|| namespaces.iter().next().map(|ns| ns.to_string()))
            .flatten(),
        details,
    }
}

/// `name@namespace` for up to twenty workloads, then a count of the rest,
/// so one huge manifest can't make a huge record.
fn summarise_names(workloads: &[(String, String)]) -> String {
    const SHOWN: usize = 20;
    let mut names: Vec<String> = workloads
        .iter()
        .take(SHOWN)
        .map(|(name, namespace)| format!("{name}@{namespace}"))
        .collect();
    if workloads.len() > SHOWN {
        names.push(format!("+{} more", workloads.len() - SHOWN));
    }
    names.join(",")
}

/// Query parameters of `GET /v1/audit`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct AuditQuery {
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    until: Option<u64>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    principal: Option<String>,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// Merge every live node's log, not just this node's.
    #[serde(default)]
    cluster: bool,
}

impl AuditQuery {
    fn filter(&self) -> AuditFilter {
        AuditFilter {
            since: self.since,
            until: self.until,
            action: self.action.clone(),
            principal: self.principal.clone(),
            app: self.app.clone(),
            namespace: self.namespace.clone(),
            limit: self.limit,
        }
    }
}

/// `GET /v1/audit`: the newest matching audit records, oldest first.
///
/// The log spans every tenant and names who holds which token, so it needs
/// an unscoped Admin that a `[permission]` block, if any, grants `admin`
/// across the cluster. The internal service principal is refused: a stolen
/// node credential mustn't be able to read who is investigating it. With
/// `?cluster=true` the node asks every live member under the caller's own
/// credential, so each repeats the check, and merges the answers by time.
pub(super) async fn audit_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize_user(auth.as_deref(), crate::sesame::types::ApiRole::Admin)
    {
        return response;
    }
    if let Err(response) = crate::sesame::auth::require_unscoped(auth.as_deref()) {
        return response;
    }
    if let Err(response) = enforce_cluster_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Admin,
    )
    .await
    {
        return response;
    }
    let filter = query.filter();
    let limit = filter.effective_limit();
    let (mut records, mut warnings) = local_audit(&state, &filter).await;
    if query.cluster {
        let (peer_records, peer_warnings) = peer_audit(&state, &headers, &filter).await;
        records.extend(peer_records);
        warnings.extend(peer_warnings);
    }
    Json(AuditListing {
        records: crate::bun::audit::newest(records, limit),
        warnings,
    })
    .into_response()
}

/// This node's matching records, and a warning if it keeps no log, can't
/// read it, or has lost writes.
async fn local_audit(state: &ApiState, filter: &AuditFilter) -> (Vec<AuditRecord>, Vec<String>) {
    let name = local_node_name(state);
    let log = match &state.events {
        Some(events) => events.read().await.audit_log().cloned(),
        None => None,
    };
    let Some(log) = log else {
        return (Vec::new(), vec![format!("node {name}: keeps no audit log")]);
    };
    let mut warnings = Vec::new();
    let failures = log.failures();
    if failures > 0 {
        warnings.push(format!(
            "node {name}: {failures} audit record(s) failed to reach the disk since it started"
        ));
    }
    match log.query(filter.clone()).await {
        Ok(records) => (records, warnings),
        Err(error) => {
            warnings.push(format!("node {name}: {error}"));
            (Vec::new(), warnings)
        }
    }
}

/// How long one peer may take to answer an audit query.
const PEER_AUDIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The URL path a peer is asked for: the same filters, its own log only.
//
// A plain function: the URL serialiser holds a non-`Send` reference, which
// must not live across an `.await` in the handler.
pub(super) fn peer_audit_path(filter: &AuditFilter) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    if let Some(since) = filter.since {
        params.append_pair("since", &since.to_string());
    }
    if let Some(until) = filter.until {
        params.append_pair("until", &until.to_string());
    }
    if let Some(action) = &filter.action {
        params.append_pair("action", action);
    }
    if let Some(principal) = &filter.principal {
        params.append_pair("principal", principal);
    }
    if let Some(app) = &filter.app {
        params.append_pair("app", app);
    }
    if let Some(namespace) = &filter.namespace {
        params.append_pair("namespace", namespace);
    }
    params.append_pair("limit", &filter.effective_limit().to_string());
    format!("/v1/audit?{}", params.finish())
}

/// Ask every other live member for its matching records, under the
/// caller's credential.
async fn peer_audit(
    state: &ApiState,
    headers: &HeaderMap,
    filter: &AuditFilter,
) -> (Vec<AuditRecord>, Vec<String>) {
    let local_name = local_node_name(state);
    let members = match &state.membership {
        Some(membership) => membership.read().await.clone(),
        None => Vec::new(),
    };
    let path = peer_audit_path(filter);
    let requests = futures_util::stream::iter(
        members
            .into_iter()
            .filter(|member| member.node_id.0 != local_name)
            .map(|member| {
                let path = path.clone();
                async move {
                    let name = member.node_id.0.clone();
                    let url = state.cluster_http.url(&member.address.to_string(), &path);
                    let request =
                        copy_forwarded_auth(state.cluster_http.client().get(url), headers);
                    let answer = tokio::time::timeout(PEER_AUDIT_TIMEOUT, async {
                        request
                            .send()
                            .await?
                            .error_for_status()?
                            .json::<AuditListing>()
                            .await
                    })
                    .await;
                    match answer {
                        Ok(Ok(listing)) => Ok(listing),
                        Ok(Err(error)) => Err(format!("node {name}: {error}")),
                        Err(_) => Err(format!("node {name} timed out")),
                    }
                }
            }),
    )
    .buffer_unordered(8);
    tokio::pin!(requests);
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    while let Some(answer) = requests.next().await {
        match answer {
            Ok(listing) => {
                records.extend(listing.records);
                warnings.extend(listing.warnings);
            }
            Err(warning) => warnings.push(warning),
        }
    }
    warnings.sort();
    (records, warnings)
}

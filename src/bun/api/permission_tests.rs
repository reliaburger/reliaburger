//! Route-level tests for `[permission]` enforcement (B18).
//!
//! `PermissionSpec::allows` returning `false` proves nothing about a handler
//! that never asks it. These tests drive real requests through the real
//! router, one per gated route in [`crate::bun::authz::ROUTE_MATRIX`] and per
//! principal, and check the handler refuses exactly the callers whose spec
//! doesn't grant the route's action.
//!
//! The principals are injected as [`AuthContext`]s by a small test layer, so
//! a single router can play every caller without paying Argon2 per request.
//! The contexts are the same shapes the auth middleware builds: a bearer
//! token, a read-only browser session (cookie), the internal system
//! principal. `bearer_and_cookie_identities_hit_the_same_log_and_metric_gates`
//! then repeats the first failing rows through the real middleware.

use super::*;
use crate::bun::authz::{self, PermissionGate, RoutePrincipal};
use crate::config::{PermissionAction, PermissionSpec};
use crate::sesame::auth::{AuthContext, SYSTEM_PRINCIPAL};
use crate::sesame::types::ApiRole;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// The app every per-app request targets, and the one a narrow grant names
/// instead.
const TARGET_APP: &str = "api";
const OTHER_APP: &str = "web";
const NAMESPACE: &str = "default";
/// Header the test layer reads to pick the injected principal.
const PRINCIPAL_HEADER: &str = "x-test-principal";

/// One concrete request for a gated route.
struct Probe {
    method: reqwest::Method,
    path: String,
    body: &'static str,
    content_type: Option<&'static str>,
    websocket: bool,
}

/// A request for `route` that reaches the handler body: path parameters
/// filled in and a body that parses far enough to pass the extractors.
/// Every body is deliberately rejected *after* authorisation (bad JSON, an
/// empty node id), so an allowed caller changes nothing.
fn probe_for(route: &authz::Route) -> Probe {
    let path = route
        .path
        .replace("{app}", TARGET_APP)
        .replace("{namespace}", NAMESPACE);
    let method = match route.method {
        authz::Method::Get => reqwest::Method::GET,
        authz::Method::Post => reqwest::Method::POST,
        authz::Method::Delete => reqwest::Method::DELETE,
    };
    let (path, body, content_type) = match route.path {
        "/v1/logs/sql" => (format!("{path}?q=SELECT%20*%20FROM%20logs"), "", None),
        "/v1/metrics/app/{app}/{namespace}/chart" => (
            format!("{path}?name=process_cpu_percent&kind=gauge"),
            "",
            None,
        ),
        "/v1/logs/export" => (
            path,
            r#"{"destination":"/nonexistent/reliaburger-export"}"#,
            Some("application/json"),
        ),
        "/v1/nodes/decommission" => (
            path,
            r#"{"node_id":"","workloads_stopped":true,"reason":"test"}"#,
            Some("application/json"),
        ),
        _ if route.method == authz::Method::Post => (path, "not json", None),
        _ => (path, "", None),
    };
    Probe {
        method,
        path,
        body,
        content_type,
        websocket: route.path.starts_with("/v1/ws/"),
    }
}

/// The routes the table drives: every row gated on a path-named app or on
/// the whole cluster. `Body` and `Filtered` gates don't refuse outright;
/// they have their own tests below.
fn refusing_routes() -> Vec<(&'static authz::Route, PermissionGate)> {
    authz::ROUTE_MATRIX
        .iter()
        .filter_map(|route| match route.permission {
            Some(gate @ (PermissionGate::App(_) | PermissionGate::Cluster(_))) => {
                Some((route, gate))
            }
            _ => None,
        })
        .filter(|(route, _)| {
            // Mutations whose "allowed" path would really stop, delete, roll
            // back or exec into a workload; their deploy/scale/exec gates
            // predate B18 and have their own tests.
            !matches!(
                route.path,
                "/v1/stop/{app}/{namespace}"
                    | "/v1/delete/{app}/{namespace}"
                    | "/v1/exec/{app}/{namespace}"
            )
        })
        .collect()
}

/// A caller: its auth context (or none, the bootstrap window) and the spec
/// written for its token name (or none).
struct Caller {
    label: String,
    context: Option<AuthContext>,
    spec: Option<PermissionSpec>,
}

fn bearer(name: &str, role: ApiRole) -> AuthContext {
    AuthContext {
        token_name: name.to_string(),
        principal_id: format!("token:{name}"),
        role,
        scoped_apps: None,
        scoped_namespaces: None,
    }
}

/// What the auth middleware builds from a browser session cookie: the
/// token's name and scope, always read-only.
fn cookie(name: &str) -> AuthContext {
    AuthContext {
        principal_id: format!("session:{name}"),
        ..bearer(name, ApiRole::ReadOnly)
    }
}

fn spec(actions: &[&str], apps: &[&str], namespaces: Option<&[&str]>) -> PermissionSpec {
    PermissionSpec {
        actions: actions.iter().map(|a| a.to_string()).collect(),
        apps: apps.iter().map(|a| a.to_string()).collect(),
        namespaces: namespaces.map(|ns| ns.iter().map(|n| n.to_string()).collect()),
    }
}

/// The least role a route's principal class admits.
fn least_role(principal: RoutePrincipal) -> ApiRole {
    match principal {
        RoutePrincipal::AnyToken => ApiRole::ReadOnly,
        RoutePrincipal::Deployer => ApiRole::Deployer,
        _ => ApiRole::Admin,
    }
}

/// Every caller for one route, with whether it must be let through.
fn callers_for(route: &authz::Route, gate: PermissionGate, index: usize) -> Vec<(Caller, bool)> {
    let (action, per_app) = match gate {
        PermissionGate::App(action) => (action, true),
        PermissionGate::Cluster(action) => (action, false),
        _ => unreachable!("only refusing gates reach the table"),
    };
    let action = action.as_str();
    let required = least_role(route.principal);
    // Token names are unique per route so all their specs can coexist in one
    // council.
    let name = |label: &str| format!("r{index}-{label}");
    let with_spec = |label: &str, role: ApiRole, spec: PermissionSpec, allowed: bool| {
        (
            Caller {
                label: label.to_string(),
                context: Some(bearer(&name(label), role)),
                spec: Some(spec),
            },
            allowed,
        )
    };

    let mut callers = vec![
        // The bootstrap window and node-to-node fan-out are never gated.
        (
            Caller {
                label: "bootstrap".into(),
                context: None,
                spec: None,
            },
            true,
        ),
    ];
    // Role alone, no spec: exactly today's behaviour for `relish` operators.
    for role in [ApiRole::ReadOnly, ApiRole::Deployer, ApiRole::Admin] {
        let label = format!("no-spec-{role}");
        callers.push((
            Caller {
                context: Some(bearer(&name(&label), role)),
                label,
                spec: None,
            },
            crate::sesame::token::check_role(role, required).is_ok(),
        ));
    }
    let role = ApiRole::Admin;
    // A grant for some other action: `deploy` (the B18 case), or `logs` on
    // the routes whose own action is `deploy`.
    let other = if action == "deploy" { "logs" } else { "deploy" };
    callers.extend([
        with_spec(
            "action-everywhere",
            role,
            spec(&[action], &["*"], None),
            true,
        ),
        with_spec(
            "admin-everywhere",
            role,
            spec(&["admin"], &["*"], None),
            true,
        ),
        with_spec(
            "action-on-target",
            role,
            spec(&[action], &[TARGET_APP], None),
            per_app,
        ),
        with_spec(
            "action-on-other-app",
            role,
            spec(&[action], &[OTHER_APP], None),
            false,
        ),
        with_spec(
            "action-in-namespace",
            role,
            spec(&[action], &["*"], Some(&[NAMESPACE])),
            per_app,
        ),
        with_spec(
            "action-in-other-namespace",
            role,
            spec(&[action], &["*"], Some(&["elsewhere"])),
            false,
        ),
        // The B18 rows: a deploy-only or empty spec used to leave every read
        // (and every admin route) wide open.
        with_spec(
            "other-action-only",
            role,
            spec(&[other], &["*"], None),
            false,
        ),
        with_spec("no-actions", role, spec(&[], &["*"], None), false),
    ]);
    // A grant can narrow a role but never widen it.
    if required != ApiRole::ReadOnly {
        callers.push(with_spec(
            "readonly-with-admin-spec",
            ApiRole::ReadOnly,
            spec(&["admin"], &["*"], None),
            false,
        ));
    }
    // A browser session is read-only: it rides the token's spec on reads.
    let session_allowed = required == ApiRole::ReadOnly;
    for (label, grant, allowed) in [
        ("cookie-with-action", [action], session_allowed),
        ("cookie-other-action-only", [other], false),
    ] {
        callers.push((
            Caller {
                context: Some(cookie(&name(label))),
                label: label.into(),
                spec: Some(spec(&grant, &["*"], None)),
            },
            allowed,
        ));
    }
    // Token scope still applies on top of the spec for per-app routes.
    if per_app {
        for (label, namespace, allowed) in [
            ("scoped-in-namespace", NAMESPACE, true),
            ("scoped-elsewhere", "elsewhere", false),
        ] {
            let mut context = bearer(&name(label), role);
            context.scoped_namespaces = Some(vec![namespace.to_string()]);
            callers.push((
                Caller {
                    label: label.into(),
                    context: Some(context),
                    spec: Some(spec(&[action], &["*"], None)),
                },
                allowed,
            ));
        }
    }
    callers
}

/// Routes that refuse the internal service principal outright (AUTH4): it
/// exists for node-to-node fan-out, never user management.
fn refuses_system_principal(path: &str) -> bool {
    matches!(
        path,
        "/v1/token/create"
            | "/v1/token/list"
            | "/v1/token/revoke"
            | "/v1/join-token/create"
            | "/v1/join-token/list"
            | "/v1/join-token/revoke"
            | "/v1/identity/sign"
            | "/v1/secret/rotate"
            | "/v1/nodes/decommission"
    )
}

/// A running router, served on a real socket so WebSocket upgrades work,
/// with a test layer that injects the principal a request names.
struct Fixture {
    address: std::net::SocketAddr,
    council: Arc<crate::council::CouncilNode>,
    principals: Arc<std::sync::RwLock<HashMap<String, AuthContext>>>,
    stop: CancellationToken,
}

impl Fixture {
    async fn start(tag: &str) -> Self {
        Self::start_with(tag, None).await
    }

    async fn start_with(tag: &str, alerts: Option<Arc<RwLock<AlertEvaluator>>>) -> Self {
        let council = super::tests::seeded_council(tag).await;
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let stop = CancellationToken::new();
        spawn_status_agent(cmd_rx, stop.clone());
        let app = router(
            cmd_tx,
            None,
            None,
            None,
            None,
            alerts,
            Some(council.clone()),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
        );
        let principals: Arc<std::sync::RwLock<HashMap<String, AuthContext>>> = Arc::default();
        let lookup = Arc::clone(&principals);
        let app = app.layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                let context = request
                    .headers()
                    .get(PRINCIPAL_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|key| lookup.read().unwrap().get(key).cloned());
                if let Some(context) = context {
                    request.extensions_mut().insert(context);
                }
                next.run(request)
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { serving.cancelled().await })
                .await
                .unwrap();
        });
        Self {
            address,
            council,
            principals,
            stop,
        }
    }

    /// Register a caller: its context for the test layer, its spec in Raft.
    /// Returns the header value that selects it.
    async fn register(&self, caller: &Caller) -> Option<String> {
        let context = caller.context.clone()?;
        if let Some(spec) = &caller.spec {
            self.council
                .write(crate::council::RaftRequest::PermissionSpec {
                    name: context.token_name.clone(),
                    spec: Box::new(spec.clone()),
                })
                .await
                .unwrap();
        }
        let key = format!("{}#{}", context.token_name, context.principal_id);
        self.principals
            .write()
            .unwrap()
            .insert(key.clone(), context);
        Some(key)
    }

    async fn status(&self, probe: &Probe, principal: Option<&str>) -> StatusCode {
        if probe.websocket {
            return self.websocket_status(probe, principal).await;
        }
        let mut request = reqwest::Client::new()
            .request(
                probe.method.clone(),
                format!("http://{}{}", self.address, probe.path),
            )
            .body(probe.body);
        if let Some(content_type) = probe.content_type {
            request = request.header("content-type", content_type);
        }
        if let Some(principal) = principal {
            request = request.header(PRINCIPAL_HEADER, principal);
        }
        let response = tokio::time::timeout(std::time::Duration::from_secs(20), request.send())
            .await
            .unwrap_or_else(|_| panic!("{} {} timed out", probe.method, probe.path))
            .unwrap();
        response.status()
    }

    /// `WebSocketUpgrade` rejects a request that isn't a real handshake
    /// before the handler runs, so the log stream needs an actual client.
    async fn websocket_status(&self, probe: &Probe, principal: Option<&str>) -> StatusCode {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}{}", self.address, probe.path)
            .into_client_request()
            .unwrap();
        if let Some(principal) = principal {
            request
                .headers_mut()
                .insert(PRINCIPAL_HEADER, principal.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((mut socket, response)) => {
                let _ = socket.close(None).await;
                response.status()
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
            Err(other) => panic!("websocket handshake failed: {other:?}"),
        }
    }

    async fn stop(self) {
        self.stop.cancel();
        self.council.shutdown().await.unwrap();
    }
}

fn instance(app: &str) -> InstanceStatus {
    InstanceStatus {
        id: format!("{NAMESPACE}/{app}-0"),
        app_name: app.to_string(),
        namespace: NAMESPACE.to_string(),
        state: "running".to_string(),
        restart_count: 0,
        host_port: None,
        exit_code: None,
        pid: Some(4242),
        runtime_unknown: false,
        status_age_ms: None,
    }
}

/// An agent that reports one `api` and one `web` instance and drops every
/// other command, so handlers that ask it anything else fail fast (500)
/// instead of hanging.
fn spawn_status_agent(mut commands: mpsc::Receiver<AgentCommand>, stop: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let command = tokio::select! {
                () = stop.cancelled() => return,
                command = commands.recv() => match command {
                    Some(command) => command,
                    None => return,
                },
            };
            if let AgentCommand::Status { response } = command {
                let _ = response.send(vec![instance(TARGET_APP), instance(OTHER_APP)]);
            }
        }
    });
}

/// The matrix: every route gated on logs, metrics, secrets or admin, against
/// every role, grant shape, session and scope. A refusal must be a 403; an
/// allowed caller must never see one.
#[tokio::test]
async fn every_gated_route_enforces_its_permission_for_every_principal() {
    let fixture = Fixture::start("b18-matrix").await;
    let routes = refusing_routes();
    // Guard against the filter matching nothing and the test "passing".
    assert!(routes.len() >= 30, "only {} gated routes", routes.len());

    let mut failures = Vec::new();
    for (index, (route, gate)) in routes.iter().enumerate() {
        let probe = probe_for(route);
        let mut callers = callers_for(route, *gate, index);
        callers.push((
            Caller {
                label: "system".into(),
                context: Some(crate::sesame::auth::system_context()),
                spec: None,
            },
            !refuses_system_principal(route.path),
        ));
        for (caller, allowed) in callers {
            let principal = fixture.register(&caller).await;
            let status = fixture.status(&probe, principal.as_deref()).await;
            let refused = status == StatusCode::FORBIDDEN;
            if refused == allowed {
                failures.push(format!(
                    "{} {} as {}: got {status}, expected {}",
                    probe.method,
                    route.path,
                    caller.label,
                    if allowed { "not 403" } else { "403" },
                ));
            }
        }
    }
    fixture.stop().await;
    assert!(
        failures.is_empty(),
        "{} permission mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The system principal is how nodes fan out log and metric reads to each
/// other; a spec named after it must not be able to gate that.
#[test]
fn fan_out_runs_as_the_system_principal_which_no_spec_can_gate() {
    let mut permissions = std::collections::BTreeMap::new();
    permissions.insert(SYSTEM_PRINCIPAL.to_string(), spec(&[], &[], None));
    let system = crate::sesame::auth::system_context();
    assert!(
        crate::sesame::auth::authorize_permission(
            Some(&system),
            PermissionAction::Logs,
            TARGET_APP,
            NAMESPACE,
            &permissions
        )
        .is_ok()
    );
    assert!(
        crate::sesame::auth::authorize_cluster_permission(
            Some(&system),
            PermissionAction::Metrics,
            &permissions
        )
        .is_ok()
    );
}

async fn get_body(fixture: &Fixture, path: &str, principal: Option<&str>) -> (StatusCode, String) {
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", fixture.address));
    if let Some(principal) = principal {
        request = request.header(PRINCIPAL_HEADER, principal);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.text().await.unwrap())
}

/// `/v1/top` reports CPU and memory per workload, so a spec without
/// `metrics` on an app leaves that app's rows out rather than failing.
#[tokio::test]
async fn top_leaves_out_workloads_the_spec_grants_no_metrics_for() {
    let fixture = Fixture::start("b18-top").await;
    let apps_in = |body: &str| {
        let rows: Vec<crate::bun::top::TopRow> = serde_json::from_str(body).unwrap();
        let mut apps: Vec<String> = rows.into_iter().map(|r| r.instance.app_name).collect();
        apps.sort();
        apps
    };
    for (label, grant, expected) in [
        ("top-no-spec", None, vec![TARGET_APP, OTHER_APP]),
        (
            "top-metrics-api",
            Some(spec(&["metrics"], &[TARGET_APP], None)),
            vec![TARGET_APP],
        ),
        (
            "top-deploy-only",
            Some(spec(&["deploy"], &["*"], None)),
            vec![],
        ),
    ] {
        let principal = fixture
            .register(&Caller {
                label: label.into(),
                context: Some(bearer(label, ApiRole::ReadOnly)),
                spec: grant,
            })
            .await;
        let (status, body) = get_body(&fixture, "/v1/top", principal.as_deref()).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        let mut expected: Vec<String> = expected.into_iter().map(str::to_string).collect();
        expected.sort();
        assert_eq!(apps_in(&body), expected, "{label}");
    }
    fixture.stop().await;
}

/// The app page embeds metric charts. Without `metrics` on the app it still
/// renders, just with no chart pointing at a metric endpoint it would be
/// refused by.
#[tokio::test]
async fn app_page_drops_metric_charts_without_the_metrics_permission() {
    let fixture = Fixture::start("b18-app-page").await;
    let path = format!("/ui/app/{TARGET_APP}/{NAMESPACE}");
    for (label, actions, charts) in [
        ("page-logs-only", &["logs"][..], false),
        ("page-metrics", &["metrics"][..], true),
    ] {
        let principal = fixture
            .register(&Caller {
                label: label.into(),
                context: Some(cookie(label)),
                spec: Some(spec(actions, &[TARGET_APP], None)),
            })
            .await;
        let (status, body) = get_body(&fixture, &path, principal.as_deref()).await;
        assert_eq!(status, StatusCode::OK, "{label}");
        assert_eq!(
            body.contains("/v1/metrics/app/"),
            charts,
            "{label}: charts present = {}",
            !charts
        );
    }
    fixture.stop().await;
}

/// Alerts are evaluated over the metric store, so the dashboard's alert
/// panel follows the same cluster-wide `metrics` grant as `/v1/alerts`.
#[tokio::test]
async fn dashboard_hides_alerts_without_a_cluster_wide_metrics_grant() {
    use crate::mayo::alert::{AlertOperator, AlertRule, AlertSeverity};
    use crate::mayo::types::MetricKey;

    let mut evaluator = AlertEvaluator::new(vec![AlertRule {
        name: "b18-cpu-high".to_string(),
        metric_name: "cpu".to_string(),
        threshold: 80.0,
        operator: AlertOperator::GreaterThan,
        for_duration: std::time::Duration::from_secs(0),
        severity: AlertSeverity::Warning,
        description: "test rule".to_string(),
    }]);
    let sample = HashMap::from([(MetricKey::with_labels("cpu", Default::default()), 95.0)]);
    evaluator.evaluate(&sample);
    evaluator.evaluate(&sample);
    assert_eq!(evaluator.firing_alerts().len(), 1);

    let fixture =
        Fixture::start_with("b18-dashboard", Some(Arc::new(RwLock::new(evaluator)))).await;
    for (label, grant, shown) in [
        ("dash-no-spec", None, true),
        ("dash-metrics", Some(spec(&["metrics"], &["*"], None)), true),
        (
            "dash-metrics-one-app",
            Some(spec(&["metrics"], &[TARGET_APP], None)),
            false,
        ),
        (
            "dash-deploy-only",
            Some(spec(&["deploy"], &["*"], None)),
            false,
        ),
    ] {
        let principal = fixture
            .register(&Caller {
                label: label.into(),
                context: Some(cookie(label)),
                spec: grant,
            })
            .await;
        let (status, body) = get_body(&fixture, "/", principal.as_deref()).await;
        assert_eq!(status, StatusCode::OK, "{label}");
        assert_eq!(body.contains("b18-cpu-high"), shown, "{label}");
    }
    fixture.stop().await;
}

/// The first failing rows from the review, through the real auth middleware
/// with a real bearer token and a real session cookie: a `deploy`-only spec
/// for `web` must not read `api`'s logs (SSE and entries) or metrics.
#[tokio::test]
async fn bearer_and_cookie_identities_hit_the_same_log_and_metric_gates() {
    let council = super::tests::seeded_council("b18-e2e").await;
    let created = crate::sesame::token::create_token(
        "ci",
        ApiRole::Deployer,
        crate::sesame::types::TokenScope::default(),
        None,
    )
    .unwrap();
    council
        .write(crate::council::RaftRequest::PermissionSpec {
            name: "ci".into(),
            spec: Box::new(spec(&["deploy"], &[OTHER_APP], None)),
        })
        .await
        .unwrap();
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let stop = CancellationToken::new();
    spawn_status_agent(cmd_rx, stop.clone());
    let store = crate::sesame::auth::new_token_store();
    store.write().await.push(created.token);
    let app = router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        Some(store),
        None,
        None,
        None,
        None,
        0,
        None,
    );

    // Exchange the token for a session cookie, as the browser login does.
    let login = app
        .clone()
        .oneshot(
            axum::http::Request::post("/ui/session")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(axum::body::Body::from(format!(
                    "token={}",
                    created.plaintext
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("login set a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    for path in [
        format!("/v1/logs/{TARGET_APP}/{NAMESPACE}"),
        format!("/v1/logs/{TARGET_APP}/{NAMESPACE}?follow=true"),
        format!("/v1/logs/entries/{TARGET_APP}/{NAMESPACE}"),
        format!("/v1/logs/query/{TARGET_APP}/{NAMESPACE}"),
        format!("/v1/metrics/app/{TARGET_APP}/{NAMESPACE}"),
    ] {
        for (label, header, value) in [
            (
                "bearer",
                "authorization",
                format!("Bearer {}", created.plaintext),
            ),
            ("cookie", "cookie", cookie.clone()),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::get(&path)
                        .header(header, &value)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{label} read {path} with a deploy-only spec"
            );
        }
    }
    stop.cancel();
    council.shutdown().await.unwrap();
}

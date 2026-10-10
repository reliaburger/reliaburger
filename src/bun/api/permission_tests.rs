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
        .replace("{namespace}", NAMESPACE)
        .replace("{name}", "nightly");
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
        "/v1/snapshots/{namespace}/{app}/restore" => {
            (path, r#"{"name":"nightly"}"#, Some("application/json"))
        }
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
            // Test leases answer only an authenticated user the cluster's
            // test policy admits; `test_leases_need_admin_permission` drives
            // them instead.
            !matches!(
                route.path,
                "/v1/stop/{app}/{namespace}"
                    | "/v1/delete/{app}/{namespace}"
                    | "/v1/exec/{app}/{namespace}"
                    | "/v1/test/leases"
                    | "/v1/test/leases/{id}/renew"
                    | "/v1/test/leases/{id}"
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
            | "/v1/token/rotate"
            | "/v1/join-token/create"
            | "/v1/identity/sign"
            | "/v1/secret/rotate"
            | "/v1/ca/rotation/prepare"
            | "/v1/ca/rotation/begin"
            | "/v1/ca/rotation/finalize"
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

/// Another tenant's namespace, for the scope-filtered reads.
const OTHER_NAMESPACE: &str = "team-b";
/// The faults the test agent holds: one on `api` here, one on another
/// tenant's `web`, and a node fault that targets no app.
const TARGET_FAULT_ID: u64 = 7;
const OTHER_FAULT_ID: u64 = 8;
const NODE_FAULT_ID: u64 = 9;

fn fault_on(id: u64, service: &str, namespace: Option<&str>) -> crate::smoker::types::FaultSummary {
    crate::smoker::types::FaultSummary {
        id,
        fault_type: "pause".to_string(),
        target_service: service.to_string(),
        namespace: namespace.map(str::to_string),
        target_instance: None,
        target_node: None,
        remaining_secs: 60,
        injected_by: "test".to_string(),
        node: None,
        routed: Vec::new(),
    }
}

fn service(app: &str, namespace: &str) -> crate::onion::types::ResolveResponse {
    crate::onion::types::ResolveResponse {
        app_name: app.to_string(),
        namespace: namespace.to_string(),
        vip: "127.128.0.1".to_string(),
        port: 8080,
        healthy_backends: 0,
        total_backends: 0,
        backends: Vec::new(),
    }
}

fn ingress_route(app: &str, namespace: &str) -> crate::wrapper::types::RouteInfo {
    crate::wrapper::types::RouteInfo {
        host: format!("{app}.example.test"),
        path: "/".to_string(),
        app_name: app.to_string(),
        namespace: namespace.to_string(),
        healthy_backends: 0,
        total_backends: 0,
        websocket: false,
    }
}

/// An agent that reports one `api` and one `web` instance, the faults,
/// services and ingress routes above, and drops every other command, so
/// handlers that ask it anything else fail fast (500) instead of hanging.
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
            match command {
                AgentCommand::Status { response } => {
                    let _ = response.send(vec![instance(TARGET_APP), instance(OTHER_APP)]);
                }
                AgentCommand::ListFaults { response } => {
                    let _ = response.send(vec![
                        fault_on(TARGET_FAULT_ID, TARGET_APP, Some(NAMESPACE)),
                        fault_on(OTHER_FAULT_ID, OTHER_APP, Some(OTHER_NAMESPACE)),
                        fault_on(NODE_FAULT_ID, "", None),
                    ]);
                }
                AgentCommand::ResolveAll { response } => {
                    let _ = response.send(vec![
                        service(TARGET_APP, NAMESPACE),
                        service(OTHER_APP, OTHER_NAMESPACE),
                    ]);
                }
                AgentCommand::Resolve { app_name, response } => {
                    let found = [
                        service(TARGET_APP, NAMESPACE),
                        service(OTHER_APP, OTHER_NAMESPACE),
                    ]
                    .into_iter()
                    .find(|entry| entry.app_name == app_name);
                    let _ = response.send(found);
                }
                AgentCommand::Routes { response } => {
                    let _ = response.send(vec![
                        ingress_route(TARGET_APP, NAMESPACE),
                        ingress_route(OTHER_APP, OTHER_NAMESPACE),
                    ]);
                }
                AgentCommand::ResolveExecutionLogs {
                    app_name,
                    instance,
                    response,
                    ..
                } => {
                    let _ = response.send(Ok(crate::bun::agent::LogExecutionSelection {
                        logical_name: app_name,
                        instances: Vec::new(),
                        selected_instance: instance,
                    }));
                }
                _ => {}
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

/// Send one request as `principal` and read the answer.
async fn send(
    fixture: &Fixture,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    principal: Option<&str>,
) -> (StatusCode, String) {
    let mut request =
        reqwest::Client::new().request(method, format!("http://{}{path}", fixture.address));
    if let Some(body) = body {
        request = request.json(&body);
    }
    if let Some(principal) = principal {
        request = request.header(PRINCIPAL_HEADER, principal);
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(20), request.send())
        .await
        .unwrap_or_else(|_| panic!("{path} timed out"))
        .unwrap();
    (response.status(), response.text().await.unwrap())
}

/// Register a bearer token of `role` whose `[permission]` block grants
/// `actions` on `apps`, and return the header value that selects it.
async fn block(
    fixture: &Fixture,
    label: &str,
    role: ApiRole,
    actions: &[&str],
    apps: &[&str],
) -> String {
    fixture
        .register(&Caller {
            label: label.into(),
            context: Some(bearer(label, role)),
            spec: Some(spec(actions, apps, None)),
        })
        .await
        .expect("a bearer caller has a header")
}

/// Whether a response is the `[permission]` refusal for `action`, rather
/// than some later check (the test policy, a missing agent) answering.
fn refused_for(status: StatusCode, body: &str, action: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains(&format!("does not grant {action}"))
}

/// D5: a block that grants only `logs` on an app no longer lets a Deployer
/// token snapshot, restore over or delete that app's volumes. `deploy` does.
#[tokio::test]
async fn logs_only_block_cannot_create_restore_or_delete_snapshots() {
    let fixture = Fixture::start("d5-snapshots").await;
    let logs_only = block(
        &fixture,
        "snap-logs",
        ApiRole::Deployer,
        &["logs"],
        &[TARGET_APP],
    )
    .await;
    let deploy = block(
        &fixture,
        "snap-deploy",
        ApiRole::Deployer,
        &["deploy"],
        &[TARGET_APP],
    )
    .await;
    let base = format!("/v1/snapshots/{NAMESPACE}/{TARGET_APP}");
    for (method, path, body) in [
        (
            reqwest::Method::POST,
            base.clone(),
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            format!("{base}/restore"),
            Some(serde_json::json!({ "name": "nightly" })),
        ),
        (reqwest::Method::DELETE, format!("{base}/nightly"), None),
    ] {
        let (status, text) = send(
            &fixture,
            method.clone(),
            &path,
            body.clone(),
            Some(&logs_only),
        )
        .await;
        assert!(
            refused_for(status, &text, "deploy"),
            "{method} {path} with logs only: {status} {text}"
        );
        let (status, text) = send(&fixture, method.clone(), &path, body, Some(&deploy)).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} with deploy: {text}"
        );
    }
    fixture.stop().await;
}

/// D5: a build pushes an app's image, so it needs `deploy` on the app its
/// destination repository names.
#[tokio::test]
async fn logs_only_block_cannot_submit_a_build() {
    let fixture = Fixture::start("d5-build").await;
    let logs_only = block(
        &fixture,
        "build-logs",
        ApiRole::Deployer,
        &["logs"],
        &[TARGET_APP],
    )
    .await;
    let other_app = block(
        &fixture,
        "build-web",
        ApiRole::Deployer,
        &["deploy"],
        &[OTHER_APP],
    )
    .await;
    let deploy = block(
        &fixture,
        "build-deploy",
        ApiRole::Deployer,
        &["deploy"],
        &[TARGET_APP],
    )
    .await;
    // A malformed context digest: an admitted build is still refused before
    // it could run anywhere.
    let body = serde_json::json!({
        "name": "api-image",
        "context_digest": "sha256:not-a-digest",
        "spec": {
            "context": ".",
            "destination": format!("pickle://{NAMESPACE}/{TARGET_APP}:v1"),
        },
    });
    for principal in [&logs_only, &other_app] {
        let (status, text) = send(
            &fixture,
            reqwest::Method::POST,
            "/v1/build",
            Some(body.clone()),
            Some(principal),
        )
        .await;
        assert!(
            refused_for(status, &text, "deploy"),
            "{principal}: {status} {text}"
        );
    }
    let (status, text) = send(
        &fixture,
        reqwest::Method::POST,
        "/v1/build",
        Some(body),
        Some(&deploy),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "deploy on the app: {text}");
    fixture.stop().await;
}

/// D5: injecting and clearing faults needs the new `fault` action, on the
/// target service for a workload fault and across the cluster for a node
/// fault or a clear that spans tenants.
#[tokio::test]
async fn fault_needs_the_fault_permission_action() {
    let fixture = Fixture::start("d5-fault").await;
    let deploy_only = block(
        &fixture,
        "fault-deploy",
        ApiRole::Deployer,
        &["deploy"],
        &["*"],
    )
    .await;
    let on_target = block(
        &fixture,
        "fault-api",
        ApiRole::Deployer,
        &["fault"],
        &[TARGET_APP],
    )
    .await;
    let workload = serde_json::json!({
        "fault_type": { "type": "Pause" },
        "target_service": TARGET_APP,
        "namespace": NAMESPACE,
        "duration": { "secs": 1, "nanos": 0 },
        "injected_by": "",
    });
    let node = serde_json::json!({
        "fault_type": { "type": "NodeDrain" },
        "target_service": "",
        "target_node": "node-x",
        "duration": { "secs": 1, "nanos": 0 },
        "injected_by": "",
    });
    let clear_one = format!("/v1/fault?service={TARGET_APP}&namespace={NAMESPACE}");
    let by_id = |id: u64| format!("/v1/fault/{id}");
    let post = reqwest::Method::POST;
    let delete = reqwest::Method::DELETE;

    // A block without `fault` refuses every inject and clear.
    for (method, path, body) in [
        (
            post.clone(),
            "/v1/fault".to_string(),
            Some(workload.clone()),
        ),
        (post.clone(), "/v1/fault".to_string(), Some(node.clone())),
        (delete.clone(), clear_one.clone(), None),
        (delete.clone(), "/v1/fault".to_string(), None),
        (delete.clone(), by_id(TARGET_FAULT_ID), None),
    ] {
        let (status, text) = send(&fixture, method.clone(), &path, body, Some(&deploy_only)).await;
        assert!(
            refused_for(status, &text, "fault"),
            "{method} {path} without fault: {status} {text}"
        );
    }
    // `fault` on one app covers that app's faults and nothing wider.
    for (method, path, body) in [
        (
            post.clone(),
            "/v1/fault".to_string(),
            Some(workload.clone()),
        ),
        (delete.clone(), clear_one.clone(), None),
        (delete.clone(), by_id(TARGET_FAULT_ID), None),
    ] {
        let (status, text) = send(&fixture, method.clone(), &path, body, Some(&on_target)).await;
        assert!(
            !refused_for(status, &text, "fault"),
            "{method} {path} with fault on the app: {text}"
        );
    }
    for (method, path, body) in [
        (post.clone(), "/v1/fault".to_string(), Some(node)),
        (delete.clone(), "/v1/fault".to_string(), None),
        (delete.clone(), by_id(OTHER_FAULT_ID), None),
        (delete.clone(), by_id(NODE_FAULT_ID), None),
    ] {
        let (status, text) = send(&fixture, method.clone(), &path, body, Some(&on_target)).await;
        assert!(
            refused_for(status, &text, "fault"),
            "{method} {path} beyond the app: {status} {text}"
        );
    }
    fixture.stop().await;
}

/// D5: test leases provision cluster resources, so a block must grant
/// `admin` across the cluster for a Deployer to create, renew or release one.
#[tokio::test]
async fn test_leases_need_admin_permission() {
    let fixture = Fixture::start("d5-leases").await;
    let deploy_only = block(
        &fixture,
        "lease-deploy",
        ApiRole::Deployer,
        &["deploy"],
        &["*"],
    )
    .await;
    let admin_one_app = block(
        &fixture,
        "lease-admin-api",
        ApiRole::Deployer,
        &["admin"],
        &[TARGET_APP],
    )
    .await;
    let admin = block(
        &fixture,
        "lease-admin",
        ApiRole::Deployer,
        &["admin"],
        &["*"],
    )
    .await;
    let requests = [
        (
            reqwest::Method::POST,
            "/v1/test/leases",
            Some(serde_json::json!({ "ttl_seconds": 60 })),
        ),
        (
            reqwest::Method::POST,
            "/v1/test/leases/lease-1/renew",
            Some(serde_json::json!({ "ttl_seconds": 60 })),
        ),
        (reqwest::Method::DELETE, "/v1/test/leases/lease-1", None),
    ];
    for (method, path, body) in requests.clone() {
        for principal in [&deploy_only, &admin_one_app] {
            let (status, text) = send(
                &fixture,
                method.clone(),
                path,
                body.clone(),
                Some(principal),
            )
            .await;
            assert!(
                refused_for(status, &text, "admin"),
                "{method} {path} as {principal}: {status} {text}"
            );
        }
        let (status, text) = send(&fixture, method.clone(), path, body, Some(&admin)).await;
        assert!(
            !refused_for(status, &text, "admin"),
            "{method} {path} with admin: {text}"
        );
    }
    fixture.stop().await;
}

/// A token scoped to one namespace, as a workload's JWT is.
async fn scoped_reader(fixture: &Fixture, label: &str) -> String {
    let mut context = bearer(label, ApiRole::ReadOnly);
    context.token_name = format!("workload:{NAMESPACE}/{TARGET_APP}");
    context.scoped_apps = Some(vec![TARGET_APP.to_string()]);
    context.scoped_namespaces = Some(vec![NAMESPACE.to_string()]);
    fixture
        .register(&Caller {
            label: label.into(),
            context: Some(context),
            spec: None,
        })
        .await
        .expect("a scoped caller has a header")
}

/// Every container's JWT is confined to its own app and namespace, so the
/// discovery reads must not list another tenant's services or routes.
#[tokio::test]
async fn resolve_and_routes_hide_other_namespaces_from_a_workload_jwt() {
    let fixture = Fixture::start("scope-discovery").await;
    let workload = scoped_reader(&fixture, "jwt-api").await;
    let unscoped = fixture
        .register(&Caller {
            label: "reader".into(),
            context: Some(bearer("reader", ApiRole::ReadOnly)),
            spec: None,
        })
        .await;
    let namespaces_in = |body: &str| {
        let rows: Vec<serde_json::Value> = serde_json::from_str(body).unwrap();
        let mut seen: Vec<String> = rows
            .iter()
            .map(|row| row["namespace"].as_str().unwrap_or_default().to_string())
            .collect();
        seen.sort();
        seen
    };
    for path in ["/v1/resolve", "/v1/routes"] {
        let (status, body) = get_body(&fixture, path, Some(&workload)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(namespaces_in(&body), vec![NAMESPACE.to_string()], "{path}");
        let (_, body) = get_body(&fixture, path, unscoped.as_deref()).await;
        assert_eq!(
            namespaces_in(&body),
            vec![NAMESPACE.to_string(), OTHER_NAMESPACE.to_string()],
            "{path} unscoped"
        );
    }
    let (status, _) = get_body(
        &fixture,
        &format!("/v1/resolve/{OTHER_APP}"),
        Some(&workload),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "resolved another tenant's service"
    );
    let (status, _) = get_body(
        &fixture,
        &format!("/v1/resolve/{TARGET_APP}"),
        Some(&workload),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_body(
        &fixture,
        &format!("/v1/resolve/{OTHER_APP}"),
        unscoped.as_deref(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    fixture.stop().await;
}

/// The cluster-wide fault list carries every tenant's faults and the node
/// faults that belong to none, so a scoped caller is refused it, and its
/// node-local list shows only its own apps' faults.
#[tokio::test]
async fn cluster_fault_list_refuses_a_scoped_caller() {
    let fixture = Fixture::start("scope-faults").await;
    let workload = scoped_reader(&fixture, "jwt-faults").await;
    let (status, body) = get_body(&fixture, "/v1/fault?cluster=true", Some(&workload)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = get_body(&fixture, "/v1/fault", Some(&workload)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let faults: Vec<crate::smoker::types::FaultSummary> = serde_json::from_str(&body).unwrap();
    let ids: Vec<u64> = faults.iter().map(|fault| fault.id).collect();
    assert_eq!(ids, vec![TARGET_FAULT_ID]);
    fixture.stop().await;
}

/// A build's status names its image and its failure output, so a scoped
/// caller may read only the builds that push into its own apps.
#[tokio::test]
async fn build_status_refuses_another_namespaces_build() {
    let fixture = Fixture::start("scope-build").await;
    let workload = scoped_reader(&fixture, "jwt-build").await;
    let mut ids = Vec::new();
    for repository in [
        Some(format!("{NAMESPACE}/{TARGET_APP}")),
        Some(format!("{OTHER_NAMESPACE}/{OTHER_APP}")),
        None,
    ] {
        let response = fixture
            .council
            .write(crate::council::RaftRequest::BuildRegister {
                build: crate::bun::build_runner::BuildRecord {
                    name: "image".to_string(),
                    runner_node: Some("elsewhere".to_string()),
                    state: crate::bun::build_runner::BuildState::Failed {
                        reason: "secret build output".to_string(),
                    },
                    created_at_epoch_secs: 1,
                    repository,
                },
            })
            .await
            .unwrap();
        let crate::council::types::CouncilResponse::BuildRegistered { build_id } = response else {
            panic!("unexpected register answer: {response:?}");
        };
        ids.push(build_id);
    }
    let (status, body) =
        get_body(&fixture, &format!("/v1/build/{}", ids[0]), Some(&workload)).await;
    assert_eq!(status, StatusCode::OK, "own build: {body}");
    for id in &ids[1..] {
        let (status, body) = get_body(&fixture, &format!("/v1/build/{id}"), Some(&workload)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "build {id}: {body}");
        assert!(!body.contains("secret build output"));
    }
    fixture.stop().await;
}

/// How far up the principal ladder a matrix row sits. A caller below a
/// row's rank must be refused; `System` is above every role because only a
/// cluster node holds the service token.
fn principal_rank(principal: RoutePrincipal) -> u8 {
    match principal {
        RoutePrincipal::Public => 0,
        RoutePrincipal::AnyToken => 1,
        RoutePrincipal::Deployer => 2,
        RoutePrincipal::Admin => 3,
        RoutePrincipal::System => 4,
    }
}

/// A request for any matrix row, every path parameter filled in. Routes
/// whose handler parses a JSON body before it can authorise get a body that
/// parses; nothing below the row's principal may ever act on it.
fn principal_probe(route: &authz::Route) -> Probe {
    let path = route
        .path
        .replace("{app}", TARGET_APP)
        .replace("{namespace}", NAMESPACE)
        .replace("{name}", "nightly")
        .replace("{node_id}", "node-x")
        .replace("{node}", "node-x")
        .replace("{index}", "0")
        .replace("{id}", "7")
        .replace("{*path}", "v1/status");
    let method = match route.method {
        authz::Method::Get => reqwest::Method::GET,
        authz::Method::Post => reqwest::Method::POST,
        authz::Method::Delete => reqwest::Method::DELETE,
    };
    let (body, content_type) = match principal_body(route.path) {
        Some(body) => (body, Some("application/json")),
        None if route.method == authz::Method::Post => ("not json", None),
        None => ("", None),
    };
    Probe {
        method,
        path,
        body,
        content_type,
        websocket: route.path.starts_with("/v1/ws/"),
    }
}

/// A body that gets a request past a route's JSON extractor to the
/// handler's own checks.
///
/// The node-to-node (`System`) routes that parse an internal message type
/// first are left out: `authz::tests::every_system_route_requires_the_system_principal`
/// proves their handlers call `require_system`, and the test below names
/// the ones it couldn't reach.
fn principal_body(path: &str) -> Option<&'static str> {
    Some(match path {
        "/v1/exec/{app}/{namespace}" => r#"{"command":["true"]}"#,
        "/v1/test/leases" | "/v1/test/leases/{id}/renew" => r#"{"ttl_seconds":60}"#,
        "/v1/snapshots/{namespace}/{app}/restore" => r#"{"name":"nightly"}"#,
        "/v1/fault" => {
            r#"{"fault_type":{"type":"Pause"},"target_service":"api","namespace":"default","duration":{"secs":1,"nanos":0},"injected_by":""}"#
        }
        "/v1/logs/export" => r#"{"destination":"/nonexistent/reliaburger-export"}"#,
        "/v1/nodes/decommission" => r#"{"node_id":"","workloads_stopped":true,"reason":"test"}"#,
        "/v1/batch/run" => r#"{"batch_id":1,"callback_base_url":null,"jobs":[]}"#,
        "/v1/batch/{id}/report" => {
            r#"{"job_name":"job","namespace":"default","exit_code":0,"status":"completed"}"#
        }
        "/v1/build/run" => {
            r#"{"name":"image","context_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","spec":{"context":".","destination":"pickle://default/api:v1"}}"#
        }
        _ => return None,
    })
}

/// Every matrix row refuses every caller below its principal class: no
/// token, a read-only token, a scoped token, a user's dashboard session, a
/// dashboard session made from the service token, and the roles in between.
/// The matrix's principal column was only ever documentation; this makes it
/// a promise. It also proves the other direction for the rows anyone may
/// read: a read-only token gets in.
#[tokio::test]
async fn every_route_refuses_callers_below_its_principal() {
    let fixture = Fixture::start("principal-matrix").await;
    let mut scoped = bearer("p-scoped", ApiRole::Deployer);
    scoped.scoped_namespaces = Some(vec!["elsewhere".to_string()]);
    let service_session =
        crate::sesame::auth::readonly_session_context(&crate::sesame::session::SessionIdentity {
            token_name: SYSTEM_PRINCIPAL.to_string(),
            principal_id: SYSTEM_PRINCIPAL.to_string(),
            scope: crate::sesame::types::TokenScope::default(),
        });
    let callers = [
        ("read-only token", bearer("p-reader", ApiRole::ReadOnly), 1),
        ("user session", cookie("p-alice"), 1),
        ("service-token session", service_session, 1),
        ("scoped token", scoped, 2),
        ("deployer token", bearer("p-deployer", ApiRole::Deployer), 2),
        ("admin token", bearer("p-admin", ApiRole::Admin), 3),
        ("system", crate::sesame::auth::system_context(), 4),
    ];
    let mut headers = Vec::new();
    for (label, context, rank) in callers {
        let header = fixture
            .register(&Caller {
                label: label.into(),
                context: Some(context),
                spec: None,
            })
            .await
            .expect("every caller has a context");
        headers.push((label, header, rank));
    }

    let mut failures = Vec::new();
    let mut unreached = std::collections::BTreeSet::new();
    for route in authz::ROUTE_MATRIX {
        let probe = principal_probe(route);
        let required = principal_rank(route.principal);
        for (label, header, rank) in &headers {
            let below = *rank < required;
            let reader_on_open_row =
                route.principal == RoutePrincipal::AnyToken && *label == "read-only token";
            if !below && !reader_on_open_row {
                continue;
            }
            let status = fixture.status(&probe, Some(header)).await;
            let refused = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
            let body_rejected = matches!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE | StatusCode::UNPROCESSABLE_ENTITY
            );
            if below && body_rejected && route.principal == RoutePrincipal::System {
                // Statically covered; see `principal_body`.
                unreached.insert(route.path);
                continue;
            }
            if below && !refused {
                failures.push(format!(
                    "{} {} ({:?}) let {label} through: {status}",
                    probe.method, route.path, route.principal
                ));
            }
            if reader_on_open_row && refused {
                failures.push(format!(
                    "{} {} is listed AnyToken but refused a read-only token: {status}",
                    probe.method, route.path
                ));
            }
        }
    }
    fixture.stop().await;

    // No token at all, through the real middleware: once any token exists,
    // every row but the public ones answers 401 before a handler runs.
    let created = crate::sesame::token::create_token(
        "someone",
        ApiRole::Admin,
        crate::sesame::types::TokenScope::default(),
        None,
    )
    .unwrap();
    let store = crate::sesame::auth::new_token_store();
    store.write().await.push(created.token);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let stop = CancellationToken::new();
    spawn_status_agent(cmd_rx, stop.clone());
    let app = router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(store),
        None,
        None,
        None,
        None,
        0,
        None,
    );
    for route in authz::ROUTE_MATRIX {
        let probe = principal_probe(route);
        let mut request = axum::http::Request::builder()
            .method(probe.method.as_str())
            .uri(&probe.path);
        if let Some(content_type) = probe.content_type {
            request = request.header("content-type", content_type);
        }
        let response = app
            .clone()
            .oneshot(request.body(axum::body::Body::from(probe.body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap_or_default();
        let middleware_refused =
            status == StatusCode::UNAUTHORIZED && body.as_ref() == b"missing authorization header";
        let public = route.principal == RoutePrincipal::Public;
        if public == middleware_refused {
            failures.push(format!(
                "{} {} ({:?}) without a token: {status} {}",
                probe.method,
                route.path,
                route.principal,
                String::from_utf8_lossy(&body)
            ));
        }
    }
    stop.cancel();

    // The node-to-node rows whose message type the probe can't build. A row
    // leaving this list is good news; a row joining it needs a body above.
    let expected_unreached = std::collections::BTreeSet::from([
        "/v1/batch/array/sync",
        "/v1/build/sign",
        "/v1/build/track",
        "/v1/chaos/fence",
        "/v1/chaos/reserve",
        "/v1/cluster/renew",
        "/v1/cluster/trust-ack",
        "/v1/cluster/workload-csr",
        "/v1/discovery/retire",
        "/v1/discovery/withdrawn",
        "/v1/registry/propose",
        "/v1/registry/query",
        "/v1/test/leases/retired",
    ]);
    assert_eq!(
        unreached, expected_unreached,
        "System rows the probe couldn't reach"
    );
    assert!(
        failures.is_empty(),
        "{} principal mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

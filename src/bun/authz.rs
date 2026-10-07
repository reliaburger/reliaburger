//! The central route → principal matrix (H4/D8 groundwork).
//!
//! Until now, every handler enforced its own role check inline, and the
//! only way to answer "who may call this route?" was to read all 4,000
//! lines of `api.rs`. That is exactly the kind of security rule that
//! drifts: add a route, forget the check, and nobody notices until a
//! review. This module lifts those decisions into one table so they can
//! be *audited in one place*.
//!
//! The table does **not** replace the per-handler checks, and it does
//! **not** add new authorisation. It records the principal each mounted
//! route requires today, and a test proves every route the router mounts
//! appears here. A new route with no matrix entry fails that test, so the
//! matrix can never silently fall behind the router.
//!
//! The *role* a route needs is only half the question; the other half is
//! **which apps** the caller's token may touch. That half lived in the
//! handlers by convention until C3 found every per-app read ignoring it,
//! so it is now a test too: see `every_per_app_route_checks_the_callers_scope`
//! below. A route pattern naming `{app}` must call `authorize_scoped`.
//!
//! There is a third question, too: what must the caller's `[permission]`
//! spec grant? Role and scope say who may read logs at all; a spec can narrow
//! a principal further, to named actions on named apps (B18). Every route that
//! reads logs or metrics, changes secrets, or performs administration records
//! the action it needs in [`Route::permission`], and the tests below prove
//! each such handler checks it.

/// The principal class a route requires.
///
/// This is coarser than [`crate::sesame::types::ApiRole`]: it also covers routes that need *no*
/// token (`Public`), any authenticated caller regardless of role
/// (`AnyToken`), and the internal cluster node identity (`System`, the
/// service-token principal that [`crate::sesame::auth::require_system`]
/// checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutePrincipal {
    /// No token required (liveness, static assets, JWKS, join, login).
    Public,
    /// Any authenticated caller (a bearer token or a session cookie).
    AnyToken,
    /// A `Deployer` or higher (deploy, stop, chaos, submit work).
    Deployer,
    /// An `Admin` token (tokens, secrets, upgrades, elections).
    Admin,
    /// The internal system principal — a cluster node presenting the
    /// service token. Node-to-node routes only.
    System,
}

/// HTTP method a matrix entry applies to. Kept as a tiny enum rather
/// than pulling `http::Method` into a `const` context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

/// What a principal's `[permission]` spec must grant for a route, on top of
/// its role and token scope.
///
/// A principal with no spec is governed by role and scope alone, so these
/// gates only bite once an operator writes a `[permission.<token-name>]`
/// block. The internal system principal (node-to-node fan-out) always passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionGate {
    /// The action on the `{app}` in `{namespace}` the path names.
    App(PermissionAction),
    /// The action on every app the request body names (a manifest, or the
    /// deploy an operation id refers to).
    Body(PermissionAction),
    /// The action across the whole cluster: `apps = ["*"]` and no
    /// `namespaces` restriction. For routes that name no single app.
    Cluster(PermissionAction),
    /// The route still answers, but leaves out the rows or page sections
    /// whose app the spec doesn't grant the action for.
    Filtered(PermissionAction),
}

/// One row of the matrix: which principal a `(method, path)` needs.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub method: Method,
    /// The axum path pattern exactly as mounted (e.g. `/v1/stop/{app}/{namespace}`).
    pub path: &'static str,
    pub principal: RoutePrincipal,
    /// The `[permission]` action the handler enforces, if any.
    pub permission: Option<PermissionGate>,
}

const fn route(method: Method, path: &'static str, principal: RoutePrincipal) -> Route {
    Route {
        method,
        path,
        principal,
        permission: None,
    }
}

/// A row whose handler also enforces a `[permission]` action.
const fn gated(
    method: Method,
    path: &'static str,
    principal: RoutePrincipal,
    gate: PermissionGate,
) -> Route {
    Route {
        method,
        path,
        principal,
        permission: Some(gate),
    }
}

use crate::config::PermissionAction;
use Method::{Delete, Get, Post};
use PermissionGate::{App, Body, Cluster, Filtered};
use RoutePrincipal::{Admin, AnyToken, Deployer, Public, System};

const ADMIN: PermissionAction = PermissionAction::Admin;
const DEPLOY: PermissionAction = PermissionAction::Deploy;
const EXEC: PermissionAction = PermissionAction::Exec;
const LOGS: PermissionAction = PermissionAction::Logs;
const METRICS: PermissionAction = PermissionAction::Metrics;
const SCALE: PermissionAction = PermissionAction::Scale;
const SECRET_WRITE: PermissionAction = PermissionAction::SecretWrite;

/// Every route the Bun API mounts, with the principal it requires.
///
/// The order mirrors `api::router` so the two are easy to diff. When you
/// add a route to the router, add it here too — `matrix_covers_every_mounted_route`
/// fails otherwise.
pub const ROUTE_MATRIX: &[Route] = &[
    // Public — no token.
    route(Get, "/v1/health", Public),
    route(Get, "/v1/version", Public),
    route(Get, "/v1/identity/jwks", Public),
    route(Get, "/ui/static/{*path}", Public),
    route(Post, "/v1/cluster/join", Public),
    route(Get, "/v1/cluster/ca", Public),
    // No bearer token: the handler requires a member's TLS client certificate.
    route(Get, "/v1/cluster/master-key", Public),
    route(Get, "/ui/login", Public),
    route(Post, "/ui/session", Public),
    route(Post, "/ui/logout", Public),
    // Dashboard + fragments — any authenticated caller (session cookie).
    gated(Get, "/", AnyToken, Filtered(METRICS)),
    gated(
        Get,
        "/ui/app/{app}/{namespace}",
        AnyToken,
        Filtered(METRICS),
    ),
    route(Get, "/ui/node/{name}", AnyToken),
    route(Get, "/ui/gitops", AnyToken),
    route(Get, "/ui/fragment/apps", AnyToken),
    route(Get, "/ui/fragment/batches", AnyToken),
    route(Get, "/ui/fragment/nodes", AnyToken),
    gated(Get, "/ui/fragment/alerts", AnyToken, Cluster(METRICS)),
    route(
        Get,
        "/ui/fragment/app/{app}/{namespace}/instances",
        AnyToken,
    ),
    route(Get, "/ui/app/{app}/{namespace}/env", AnyToken),
    // Workload lifecycle.
    gated(Post, "/v1/apply", Deployer, Body(DEPLOY)),
    route(Get, "/v1/status", AnyToken),
    route(Get, "/v1/apps", AnyToken),
    route(Get, "/v1/readiness", AnyToken),
    route(Get, "/v1/jobs", AnyToken),
    route(Get, "/v1/jobs/definitions", AnyToken),
    gated(Post, "/v1/jobs/runs", Deployer, Body(DEPLOY)),
    // Replay additionally requires a user principal and exact side-effect acknowledgement.
    gated(Post, "/v1/jobs/runs/{id}/replay", Deployer, Body(DEPLOY)),
    gated(
        Post,
        "/v1/jobs/definitions/{name}/{namespace}/disable",
        Deployer,
        Body(DEPLOY),
    ),
    route(Get, "/v1/events", AnyToken),
    route(Get, "/v1/ws/events", AnyToken),
    gated(Get, "/v1/ws/logs/{app}/{namespace}", AnyToken, App(LOGS)),
    route(Get, "/v1/status/{app}/{namespace}", AnyToken),
    gated(Get, "/v1/top", AnyToken, Filtered(METRICS)),
    gated(Post, "/v1/stop/{app}/{namespace}", Deployer, App(SCALE)),
    gated(Post, "/v1/delete/{app}/{namespace}", Deployer, App(DEPLOY)),
    gated(Get, "/v1/logs/{app}/{namespace}", AnyToken, App(LOGS)),
    gated(
        Get,
        "/v1/logs/entries/{app}/{namespace}",
        AnyToken,
        App(LOGS),
    ),
    gated(Get, "/v1/logs/query/{app}/{namespace}", AnyToken, App(LOGS)),
    gated(Post, "/v1/exec/{app}/{namespace}", Deployer, App(EXEC)),
    // Cluster + upgrade.
    // Renewal additionally requires the existing node TLS peer certificate.
    route(Post, "/v1/cluster/renew", System),
    // As does a trust acknowledgement (F04 R4).
    route(Post, "/v1/cluster/trust-ack", System),
    route(Post, "/v1/cluster/workload-csr", System),
    route(Post, "/v1/registry/propose", System),
    route(Post, "/v1/registry/query", System),
    route(Get, "/v1/capabilities", AnyToken),
    route(Get, "/v1/capabilities/cluster", AnyToken),
    route(Get, "/v1/diagnostics", AnyToken),
    route(Get, "/v1/diagnostics/apps", AnyToken),
    route(Post, "/v1/path", AnyToken),
    route(Post, "/v1/test/leases", Deployer),
    route(Get, "/v1/test/leases/{id}", AnyToken),
    route(Post, "/v1/test/leases/{id}/renew", Deployer),
    route(Delete, "/v1/test/leases/{id}", Deployer),
    route(Get, "/v1/cluster/nodes", AnyToken),
    // The relay only authenticates; the target node applies the forwarded
    // route's own requirement to the caller's credential.
    route(Get, "/v1/nodes/{node}/relay/{*path}", AnyToken),
    route(Post, "/v1/nodes/{node}/relay/{*path}", AnyToken),
    route(Get, "/v1/cluster/council", AnyToken),
    // Upgrades and elections act on the whole cluster: the handlers also
    // refuse a scoped Admin (`authorize_cluster_admin`).
    gated(Post, "/v1/upgrade/apply", Admin, Cluster(ADMIN)),
    route(Get, "/v1/upgrade/status", AnyToken),
    gated(Post, "/v1/upgrade/rollback", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/upgrade/start", Admin, Cluster(ADMIN)),
    route(Get, "/v1/upgrade/cluster", AnyToken),
    gated(Post, "/v1/upgrade/resume", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/upgrade/abort", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/upgrade/cluster-rollback", Admin, Cluster(ADMIN)),
    // Appliance OS updates: the leader stages on each node as the system
    // principal; operators start and steer the rollout.
    gated(Post, "/v1/os/stage", Admin, Cluster(ADMIN)),
    route(Get, "/v1/os/rollout", AnyToken),
    gated(Post, "/v1/os/rollout/start", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/os/rollout/resume", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/os/rollout/abort", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/cluster/elect", Admin, Cluster(ADMIN)),
    // Chaos.
    route(Post, "/v1/chaos/reserve", System),
    route(Post, "/v1/chaos/fence", System),
    route(Get, "/v1/chaos/status", AnyToken),
    // Snapshots. Reads need any token, mutations a Deployer; both are
    // additionally held to the token's app/namespace scope in the handlers.
    route(Get, "/v1/snapshots/{namespace}/{app}", AnyToken),
    route(Post, "/v1/snapshots/{namespace}/{app}", Deployer),
    route(Post, "/v1/snapshots/{namespace}/{app}/restore", Deployer),
    route(Delete, "/v1/snapshots/{namespace}/{app}/{name}", Deployer),
    // Fault injection.
    route(Post, "/v1/fault", Deployer),
    route(Delete, "/v1/fault", Deployer),
    route(Get, "/v1/fault", AnyToken),
    route(Delete, "/v1/fault/{id}", Deployer),
    // Discovery + routing.
    route(Post, "/v1/discovery/retire", System),
    route(Post, "/v1/discovery/withdrawn", System),
    route(Get, "/v1/resolve", AnyToken),
    route(Get, "/v1/resolve/{name}", AnyToken),
    route(Get, "/v1/routes", AnyToken),
    // Metrics + logs + deploys.
    gated(Get, "/v1/metrics", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/metrics/summary", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/metrics/keys", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/metrics/rollup", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/metrics/rollup/owned", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/metrics/cluster", AnyToken, Cluster(METRICS)),
    gated(
        Get,
        "/v1/metrics/app/{app}/{namespace}",
        AnyToken,
        App(METRICS),
    ),
    gated(
        Get,
        "/v1/metrics/app/{app}/{namespace}/chart",
        AnyToken,
        App(METRICS),
    ),
    gated(Get, "/v1/alerts", AnyToken, Cluster(METRICS)),
    gated(Get, "/v1/logs/sql", AnyToken, Cluster(LOGS)),
    gated(Post, "/v1/logs/export", Admin, Cluster(ADMIN)),
    route(Get, "/v1/deploys/active", AnyToken),
    route(Get, "/v1/deploys/operations", AnyToken),
    gated(
        Post,
        "/v1/deploys/operations/{id}/cancel",
        Deployer,
        Body(DEPLOY),
    ),
    route(Get, "/v1/deploys/history/{app}", AnyToken),
    gated(
        Post,
        "/v1/rollback/{app}/{namespace}",
        Deployer,
        App(DEPLOY),
    ),
    route(Get, "/v1/placements/{node_id}", AnyToken),
    route(Post, "/v1/test/leases/retired", System),
    gated(Post, "/v1/nodes/decommission", Admin, Cluster(ADMIN)),
    route(Get, "/v1/images", AnyToken),
    // Batch + build. `run`/`report`/`track`/`sign` are node-to-node (System).
    route(Post, "/v1/batch", Deployer),
    route(Post, "/v1/batch/run", System),
    route(Post, "/v1/batch/{id}/report", System),
    route(Get, "/v1/batch/{id}", AnyToken),
    // Task arrays. `sync` and the `local` reads are the leader's calls to
    // nodes; results and logs are also held to the array's scope.
    route(Post, "/v1/batch/array", Deployer),
    route(Post, "/v1/batch/manifest", Deployer),
    route(Get, "/v1/batch/summaries", AnyToken),
    route(Post, "/v1/batch/array/sync", System),
    route(Get, "/v1/batch/array/{id}/local/results", System),
    route(Get, "/v1/batch/array/{id}/local/tasks/{index}/logs", System),
    route(Post, "/v1/batch/{id}/cancel", Deployer),
    route(Get, "/v1/batch/{id}/results", AnyToken),
    route(Get, "/v1/batch/{id}/tasks/{index}/logs", AnyToken),
    route(Post, "/v1/build", Deployer),
    route(Post, "/v1/build/run", System),
    route(Post, "/v1/build/track", System),
    route(Post, "/v1/build/sign", System),
    route(Get, "/v1/build/{id}", AnyToken),
    // GitOps + identity + tokens + secrets.
    route(Post, "/v1/gitops/webhook", AnyToken),
    // Operator-only (Admin). The service principal is refused here (AUTH4),
    // so despite being a signing route it isn't a node-to-node one.
    gated(Post, "/v1/identity/sign", Admin, Cluster(ADMIN)),
    // Credential and trust management additionally requires an unscoped user.
    gated(Post, "/v1/token/create", Admin, Cluster(ADMIN)),
    // With `local=true` the system principal may also read it: that's the
    // node fan-out asking a peer for its last-use times (F05 I2).
    gated(Get, "/v1/token/list", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/token/revoke", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/token/rotate", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/join-token/create", Admin, Cluster(ADMIN)),
    gated(Get, "/v1/join-token/list", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/join-token/revoke", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/perimeter/admit", Admin, Cluster(ADMIN)),
    route(Get, "/v1/secret/public-key", AnyToken),
    gated(Post, "/v1/secret/rotate", Admin, Cluster(SECRET_WRITE)),
    // Rotating an intermediate CA (F04 R4): the operator's root signs, so
    // only an unscoped Admin may start, submit or finalise one.
    gated(Post, "/v1/ca/rotation/prepare", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/ca/rotation/begin", Admin, Cluster(ADMIN)),
    gated(Post, "/v1/ca/rotation/finalize", Admin, Cluster(ADMIN)),
];

/// Look up the principal a `(method, path)` requires, if the matrix
/// knows it. `path` is the axum pattern, not a concrete request path.
pub fn required_principal(method: Method, path: &str) -> Option<RoutePrincipal> {
    ROUTE_MATRIX
        .iter()
        .find(|r| r.method == method && r.path == path)
        .map(|r| r.principal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn required_principal_finds_known_routes() {
        assert_eq!(
            required_principal(Method::Post, "/v1/build/run"),
            Some(RoutePrincipal::System)
        );
        assert_eq!(
            required_principal(Method::Post, "/v1/apply"),
            Some(RoutePrincipal::Deployer)
        );
        assert_eq!(
            required_principal(Method::Get, "/v1/health"),
            Some(RoutePrincipal::Public)
        );
        assert_eq!(required_principal(Method::Get, "/v1/nope"), None);
    }

    #[test]
    fn matrix_has_no_duplicate_rows() {
        let mut seen = HashSet::new();
        for row in ROUTE_MATRIX {
            let method = format!("{:?}", row.method);
            assert!(
                seen.insert((method, row.path)),
                "duplicate matrix row for {} {}",
                row.path,
                row.path
            );
        }
    }

    #[test]
    fn node_to_node_routes_require_the_system_principal() {
        for path in [
            "/v1/cluster/renew",
            "/v1/cluster/trust-ack",
            "/v1/batch/run",
            "/v1/batch/{id}/report",
            "/v1/batch/array/sync",
            "/v1/build/run",
            "/v1/build/track",
            "/v1/build/sign",
        ] {
            assert_eq!(
                required_principal(Method::Post, path),
                Some(RoutePrincipal::System),
                "{path} must be a node-to-node route"
            );
        }
    }

    #[test]
    fn route_scan_tracks_chained_methods_and_ignores_comments_and_strings() {
        let source = r#"fn routes() {
            // router.route("/comment", post(handler));
            let text = ".route(\"/string\", post(handler))";
            router.route("/v1/status", get(read).post(write))
                .route("/v1/health", axum::routing::get(health))
                .route("/v1/cluster/renew", post(renew).layer(body_limit))
                .route("/layered", get(read).route_layer(auth).post(write));
        }"#;
        assert_eq!(
            mounted_route_methods(source),
            vec![
                ("get".into(), "/v1/status".into()),
                ("post".into(), "/v1/status".into()),
                ("get".into(), "/v1/health".into()),
                ("post".into(), "/v1/cluster/renew".into()),
                ("get".into(), "/layered".into()),
                ("post".into(), "/layered".into()),
            ]
        );
        assert_eq!(required_principal(Method::Post, "/v1/status"), None);
    }

    #[test]
    fn route_scan_still_refuses_unrecognised_method_wrappers() {
        assert!(
            std::panic::catch_unwind(|| mounted_route_methods(
                r#"fn routes() { router.route("/hidden", get(read).unknown_wrapper(handler)); }"#,
            ))
            .is_err()
        );
    }

    fn mounted_route_methods(source: &str) -> Vec<(String, String)> {
        use syn::visit::Visit;
        fn methods(expression: &syn::Expr, found: &mut Vec<String>) {
            let method = match expression {
                syn::Expr::Call(call) => match call.func.as_ref() {
                    syn::Expr::Path(path) => path.path.segments.last().unwrap().ident.to_string(),
                    _ => panic!("unrecognised route method expression"),
                },
                syn::Expr::MethodCall(call) => {
                    methods(&call.receiver, found);
                    if call.method == "layer" || call.method == "route_layer" {
                        return;
                    }
                    call.method.to_string()
                }
                syn::Expr::Paren(paren) => return methods(&paren.expr, found),
                _ => panic!("route methods must be explicit in the audited router"),
            };
            assert!(
                matches!(
                    method.as_str(),
                    "get" | "post" | "delete" | "put" | "patch" | "head" | "options" | "trace"
                ),
                "unrecognised route method: {method}"
            );
            found.push(method);
        }
        #[derive(Default)]
        struct Routes(Vec<(String, String)>);
        impl<'ast> Visit<'ast> for Routes {
            fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
                syn::visit::visit_expr_method_call(self, call);
                if call.method != "route" {
                    return;
                }
                assert_eq!(call.args.len(), 2, "route must have path and method router");
                let syn::Expr::Lit(path) = &call.args[0] else {
                    panic!("route path must be literal");
                };
                let syn::Lit::Str(path) = &path.lit else {
                    panic!("route path must be a string");
                };
                let mut found = Vec::new();
                methods(&call.args[1], &mut found);
                self.0
                    .extend(found.into_iter().map(|method| (method, path.value())));
            }
        }
        let file = syn::parse_file(source).expect("router source must parse");
        let mut routes = Routes::default();
        routes.visit_file(&file);
        routes.0
    }

    /// Pair each mounted route path with the handler idents it dispatches
    /// to, by reading the `get(...)`/`post(...)`/`delete(...)` calls in the
    /// same `.route(...)` fragment.
    fn mounted_route_handlers(source: &str) -> Vec<(String, Vec<String>)> {
        let mut routes = Vec::new();
        for fragment in source.split(".route(").skip(1) {
            let Some(open) = fragment.find('"') else {
                continue;
            };
            let rest = &fragment[open + 1..];
            let Some(close) = rest.find('"') else {
                continue;
            };
            let path = rest[..close].to_string();
            // The fragment runs to the next `.route(`, so everything after
            // the path belongs to this route's method handlers. Only
            // `*_handler` idents count, so an unrelated `.get(` on a map in
            // the same fragment isn't mistaken for a route handler.
            let args = &rest[close..];
            let mut handlers = Vec::new();
            for pattern in ["get(", "post(", "delete("] {
                for (index, _) in args.match_indices(pattern) {
                    let ident: String = args[index + pattern.len()..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                        .collect();
                    if ident.ends_with("_handler") {
                        handlers.push(ident);
                    }
                }
            }
            routes.push((path, handlers));
        }
        routes
    }

    /// The body of `async fn {ident}`, from its signature to the closing
    /// brace at column zero.
    fn handler_body<'a>(source: &'a str, ident: &str) -> Option<&'a str> {
        let start = source.find(&format!("async fn {ident}("))?;
        let rest = &source[start..];
        let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    /// api.rs mounts handlers that live in its route modules, one per
    /// route group. A handler the router names that none of these files
    /// defines fails the scans below, so a new route module must be listed.
    const API_ROUTE_MODULES: &[&str] = &[
        include_str!("api/apply.rs"),
        include_str!("api/apps.rs"),
        include_str!("api/ca.rs"),
        include_str!("api/deploys.rs"),
        include_str!("api/discovery.rs"),
        include_str!("api/faults.rs"),
        include_str!("api/gitops.rs"),
        include_str!("api/identity.rs"),
        include_str!("api/internal.rs"),
        include_str!("api/join.rs"),
        include_str!("api/logs.rs"),
        include_str!("api/metrics.rs"),
        include_str!("api/node_info.rs"),
        include_str!("api/nodes.rs"),
        include_str!("api/os.rs"),
        include_str!("api/registry.rs"),
        include_str!("api/secrets.rs"),
        include_str!("api/snapshots.rs"),
        include_str!("api/status.rs"),
        include_str!("api/test_leases.rs"),
        include_str!("api/ui.rs"),
        include_str!("api/upgrade.rs"),
    ];

    /// A mounted handler's body, from the file that mounts it or from one
    /// of api.rs's route modules.
    fn mounted_handler_body<'a>(source: &'a str, ident: &str) -> Option<&'a str> {
        let ident = ident.strip_prefix("super::").unwrap_or(ident);
        if let Some(ident) = ident.strip_prefix("job_api::") {
            return handler_body(include_str!("job_api.rs"), ident);
        }
        std::iter::once(source)
            .chain(API_ROUTE_MODULES.iter().copied())
            .find_map(|candidate| handler_body(candidate, ident))
    }

    /// Any route whose path names an app must check the caller's *scope*,
    /// not just its role.
    ///
    /// This is the C3 guard. Role checks were universal from the start;
    /// scope checks were on mutations only, so every per-app read handed a
    /// scoped token another tenant's data. Enforcing it by convention is
    /// exactly what failed, so the rule is a test: name an app in a route
    /// pattern and your handler must call `authorize_scoped`.
    #[test]
    fn every_per_app_route_checks_the_callers_scope() {
        let sources = [
            include_str!("api.rs"),
            include_str!("batch.rs"),
            include_str!("build_runner.rs"),
        ];
        let mut unscoped = Vec::new();
        let mut checked = 0;
        for source in sources {
            for (path, handlers) in mounted_route_handlers(source) {
                if !path.contains("{app}") {
                    continue;
                }
                for handler in handlers {
                    let Some(body) = mounted_handler_body(source, &handler) else {
                        panic!("route {path} dispatches to {handler}, which we cannot find");
                    };
                    checked += 1;
                    if !body.contains("authorize_scoped") {
                        unscoped.push(format!("{path} → {handler}"));
                    }
                }
            }
        }
        // Guard against the scan silently matching nothing and "passing".
        assert!(
            checked >= 10,
            "only found {checked} per-app handlers — the source scan is broken, not the code"
        );
        assert!(
            unscoped.is_empty(),
            "per-app routes that never check the token's scope (C3): {unscoped:?}"
        );
    }

    fn handler_checks_permission_action(
        body: &str,
        action: PermissionAction,
        shared_auth_source: &str,
    ) -> bool {
        let marker = format!("PermissionAction::{action:?}");
        if body.contains(&marker)
            || (action == PermissionAction::Admin && body.contains("authorize_cluster_admin"))
        {
            return true;
        }
        // Only Deploy is unconditional in the one shared workload rule.
        // HostExec remains conditional and cannot satisfy another route gate.
        if action != PermissionAction::Deploy {
            return false;
        }
        let Ok(auth_file) = syn::parse_file(shared_auth_source) else {
            return false;
        };
        let Some(helper) = auth_file.items.iter().find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "authorize_workload" => Some(function),
            _ => None,
        }) else {
            return false;
        };
        let shared_deploy = helper.block.stmts.iter().any(|statement| {
            let syn::Stmt::Expr(syn::Expr::Try(checked), _) = statement else {
                return false;
            };
            let syn::Expr::Call(call) = checked.expr.as_ref() else {
                return false;
            };
            let syn::Expr::Path(function) = call.func.as_ref() else {
                return false;
            };
            if !function.path.is_ident("authorize_permission") || call.args.len() != 5 {
                return false;
            }
            let Some(syn::Expr::Path(permission)) = call.args.iter().nth(1) else {
                return false;
            };
            permission
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .eq(["crate", "config", "PermissionAction", "Deploy"])
        });
        if !shared_deploy {
            return false;
        }
        let Ok(handler) = syn::parse_str::<syn::ItemFn>(body) else {
            return false;
        };
        struct SharedWorkloadGuard(bool);
        impl<'ast> syn::visit::Visit<'ast> for SharedWorkloadGuard {
            fn visit_expr_if(&mut self, expression: &'ast syn::ExprIf) {
                let recognized = (|| {
                    let syn::Expr::Let(condition) = expression.cond.as_ref() else {
                        return false;
                    };
                    let syn::Pat::TupleStruct(pattern) = condition.pat.as_ref() else {
                        return false;
                    };
                    if !pattern.path.is_ident("Err") || pattern.elems.len() != 1 {
                        return false;
                    }
                    let Some(syn::Pat::Ident(binding)) = pattern.elems.first() else {
                        return false;
                    };
                    let syn::Expr::Call(call) = condition.expr.as_ref() else {
                        return false;
                    };
                    let syn::Expr::Path(function) = call.func.as_ref() else {
                        return false;
                    };
                    if call.args.len() != 5
                        || !function
                            .path
                            .segments
                            .iter()
                            .map(|segment| segment.ident.to_string())
                            .eq(["crate", "sesame", "auth", "authorize_workload"])
                        || expression.else_branch.is_some()
                        || expression.then_branch.stmts.len() != 1
                    {
                        return false;
                    }
                    let syn::Stmt::Expr(syn::Expr::Return(returned), _) =
                        &expression.then_branch.stmts[0]
                    else {
                        return false;
                    };
                    matches!(returned.expr.as_deref(), Some(syn::Expr::Path(value)) if value.path.is_ident(&binding.ident))
                })();
                self.0 |= recognized;
                syn::visit::visit_expr_if(self, expression);
            }
        }
        let mut guard = SharedWorkloadGuard(false);
        syn::visit::Visit::visit_block(&mut guard, &handler.block);
        guard.0
    }

    #[test]
    fn permission_guardian_accepts_actual_shared_deploy_check() {
        let body = handler_body(include_str!("api/apply.rs"), "apply_handler")
            .expect("actual apply handler");
        assert!(handler_checks_permission_action(
            body,
            PermissionAction::Deploy,
            include_str!("../sesame/auth.rs"),
        ));
    }

    #[test]
    fn permission_guardian_requires_exact_error_returning_shared_call() {
        let actual = handler_body(include_str!("api/apply.rs"), "apply_handler")
            .expect("actual apply handler");
        let auth = include_str!("../sesame/auth.rs");
        for replacement in [
            "crate::sesame::auth::unrecognized_workload",
            "authorize_workload",
            "other::authorize_workload",
        ] {
            let body = actual.replace("crate::sesame::auth::authorize_workload", replacement);
            assert!(!handler_checks_permission_action(
                &body,
                PermissionAction::Deploy,
                auth
            ));
        }
        let ignored = "async fn handler() { let _ = crate::sesame::auth::authorize_workload(ctx, app, ns, host, permissions); }";
        assert!(!handler_checks_permission_action(
            ignored,
            PermissionAction::Deploy,
            auth
        ));
        assert!(!handler_checks_permission_action(
            "async fn handler() { /* crate::sesame::auth::authorize_workload(ctx, app, ns, host, permissions) */ }",
            PermissionAction::Deploy,
            auth,
        ));
        let swallowed = actual.replace("return response;", "let _ = response;");
        assert!(!handler_checks_permission_action(
            &swallowed,
            PermissionAction::Deploy,
            auth
        ));
    }

    #[test]
    fn permission_guardian_requires_real_unconditional_shared_deploy_permission() {
        let body = handler_body(include_str!("api/apply.rs"), "apply_handler")
            .expect("actual apply handler");
        for helper in [
            "pub fn authorize_workload() { Ok(()) }",
            "pub fn authorize_workload() { authorize_permission(ctx, crate::config::PermissionAction::HostExec, app, namespace, permissions)?; Ok(()) }",
            "pub fn authorize_workload() { unrelated_permission(ctx, crate::config::PermissionAction::Deploy, app, namespace, permissions)?; Ok(()) }",
            "pub fn authorize_workload() { if host { authorize_permission(ctx, crate::config::PermissionAction::Deploy, app, namespace, permissions)?; } Ok(()) }",
            "pub fn authorize_workload() { let _ = authorize_permission(ctx, crate::config::PermissionAction::Deploy, app, namespace, permissions); Ok(()) }",
            "pub fn other() { authorize_permission(ctx, crate::config::PermissionAction::Deploy, app, namespace, permissions)?; Ok(()) }",
        ] {
            assert!(!handler_checks_permission_action(
                body,
                PermissionAction::Deploy,
                helper
            ));
        }
    }

    #[test]
    fn permission_guardian_does_not_credit_optional_host_execution_or_other_actions() {
        // The actual apply handler separately checks Admin for namespace and
        // permission mutations. This isolated guard tests only the shared call.
        let body = "async fn handler() { if let Err(response) = crate::sesame::auth::authorize_workload(ctx, app, namespace, host, permissions) { return response; } }";
        for action in [
            PermissionAction::HostExec,
            PermissionAction::Exec,
            PermissionAction::Admin,
        ] {
            assert!(!handler_checks_permission_action(
                body,
                action,
                include_str!("../sesame/auth.rs"),
            ));
        }
    }

    #[test]
    fn permission_guardian_retains_direct_action_and_cluster_admin_checks() {
        assert!(handler_checks_permission_action(
            "PermissionAction::Exec",
            PermissionAction::Exec,
            "",
        ));
        assert!(handler_checks_permission_action(
            "authorize_cluster_admin",
            PermissionAction::Admin,
            "",
        ));
        assert!(!handler_checks_permission_action(
            "PermissionAction::Exec",
            PermissionAction::Deploy,
            "",
        ));
        assert!(!handler_checks_permission_action(
            "authorize_cluster_admin",
            PermissionAction::Deploy,
            "",
        ));
    }

    /// Every route the matrix gates on a `[permission]` action must name that
    /// action in its handler (B18).
    ///
    /// This is the cheap static half of the guard: it catches a new gated row
    /// whose handler forgot the check. The behavioural half, a request per
    /// route × principal, lives in `api::permission_tests`.
    #[test]
    fn every_gated_route_checks_its_permission_action() {
        let sources = [
            include_str!("api.rs"),
            include_str!("batch.rs"),
            include_str!("build_runner.rs"),
        ];
        let mut unchecked = Vec::new();
        let mut checked = 0;
        for row in ROUTE_MATRIX {
            let Some(gate) = row.permission else {
                continue;
            };
            let action = match gate {
                PermissionGate::App(action)
                | PermissionGate::Body(action)
                | PermissionGate::Cluster(action)
                | PermissionGate::Filtered(action) => action,
            };
            let marker = format!("PermissionAction::{action:?}");
            let handlers: Vec<(&str, String)> = sources
                .iter()
                .flat_map(|source| {
                    mounted_route_handlers(source)
                        .into_iter()
                        .filter(|(path, _)| path == row.path)
                        .flat_map(|(_, handlers)| handlers)
                        .map(move |handler| (*source, handler))
                })
                .collect();
            assert!(!handlers.is_empty(), "no handler found for {}", row.path);
            for (source, handler) in handlers {
                let body = mounted_handler_body(source, &handler)
                    .unwrap_or_else(|| panic!("{} dispatches to missing {handler}", row.path));
                checked += 1;
                if !handler_checks_permission_action(
                    body,
                    action,
                    include_str!("../sesame/auth.rs"),
                ) {
                    unchecked.push(format!("{} → {handler} ({marker})", row.path));
                }
            }
        }
        assert!(checked >= 30, "only found {checked} gated handlers");
        assert!(
            unchecked.is_empty(),
            "gated routes whose handler never checks the permission action: {unchecked:?}"
        );
    }

    /// Audit events span every tenant and their routes name no app, so the
    /// per-app scan above can't see them. Both the listing and its live
    /// stream must refuse a scoped token; the stream once didn't.
    #[test]
    fn both_event_routes_refuse_scoped_tokens() {
        let source = include_str!("api.rs");
        for handler in ["events_handler", "ws_events_handler"] {
            let body = mounted_handler_body(source, handler).expect(handler);
            assert!(
                body.contains("require_unscoped"),
                "{handler} serves every tenant's events to a scoped token"
            );
        }
    }

    /// F05 I1: the routes that change who can do what record who called
    /// them. A new handler for minting or revoking credentials, or for
    /// rotating keys, belongs in this list.
    #[test]
    fn trust_changing_routes_record_an_audit_event() {
        let sources = [include_str!("api.rs")];
        for handler in [
            "token_create_handler",
            "token_revoke_handler",
            "token_rotate_handler",
            "join_token_create_handler",
            "secret_rotate_handler",
        ] {
            let body = sources
                .iter()
                .find_map(|source| mounted_handler_body(source, handler))
                .unwrap_or_else(|| panic!("{handler} not found"));
            assert!(
                body.contains("record_caller_audit"),
                "{handler} changes trust without an audit event"
            );
        }
    }

    /// `/v1/logs/sql` reads across every tenant and takes no app to scope
    /// against, so it must refuse a scoped token outright.
    #[test]
    fn cluster_wide_log_sql_refuses_scoped_tokens() {
        let source = include_str!("api.rs");
        let body = mounted_handler_body(source, "logs_sql_handler").expect("logs_sql_handler");
        assert!(
            body.contains("require_unscoped"),
            "/v1/logs/sql must refuse scoped tokens — it cannot filter arbitrary SQL by tenant"
        );
    }

    /// Upgrades, rollbacks and elections change every node, so a token
    /// scoped to some apps or namespaces must not reach them.
    #[test]
    fn cluster_wide_upgrade_and_election_routes_refuse_scoped_tokens() {
        let source = include_str!("api.rs");
        for handler in [
            "upgrade_apply_handler",
            "upgrade_rollback_handler",
            "upgrade_start_handler",
            "upgrade_resume_handler",
            "upgrade_abort_handler",
            "upgrade_cluster_rollback_handler",
            "cluster_elect_handler",
            "os_stage_handler",
            "os_rollout_start_handler",
            "os_rollout_resume_handler",
            "os_rollout_abort_handler",
        ] {
            let body = mounted_handler_body(source, handler).expect(handler);
            assert!(
                body.contains("authorize_cluster_admin"),
                "{handler} must require an unscoped Admin"
            );
        }
    }

    /// Every route the router mounts must be present in the matrix. This
    /// is the guard that keeps the matrix honest: add a `.route(...)` and
    /// forget the matrix entry, and this fails.
    #[test]
    fn matrix_covers_every_mounted_route() {
        let matrix_routes: HashSet<(String, String)> = ROUTE_MATRIX
            .iter()
            .map(|row| {
                (
                    format!("{:?}", row.method).to_lowercase(),
                    row.path.to_string(),
                )
            })
            .collect();
        let sources = [
            include_str!("api.rs"),
            include_str!("batch.rs"),
            include_str!("build_runner.rs"),
        ];
        let mut missing = Vec::new();
        for source in sources {
            for route in mounted_route_methods(source) {
                if !matrix_routes.contains(&route) {
                    missing.push(route);
                }
            }
        }
        assert!(
            missing.is_empty(),
            "routes mounted but absent from the authz matrix: {missing:?}"
        );
    }
}

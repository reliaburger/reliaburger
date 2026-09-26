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

/// One row of the matrix: which principal a `(method, path)` needs.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub method: Method,
    /// The axum path pattern exactly as mounted (e.g. `/v1/stop/{app}/{namespace}`).
    pub path: &'static str,
    pub principal: RoutePrincipal,
}

const fn route(method: Method, path: &'static str, principal: RoutePrincipal) -> Route {
    Route {
        method,
        path,
        principal,
    }
}

use Method::{Delete, Get, Post};
use RoutePrincipal::{Admin, AnyToken, Deployer, Public, System};

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
    route(Get, "/ui/login", Public),
    route(Post, "/ui/session", Public),
    route(Post, "/ui/logout", Public),
    // Dashboard + fragments — any authenticated caller (session cookie).
    route(Get, "/", AnyToken),
    route(Get, "/ui/app/{app}/{namespace}", AnyToken),
    route(Get, "/ui/node/{name}", AnyToken),
    route(Get, "/ui/gitops", AnyToken),
    route(Get, "/ui/fragment/apps", AnyToken),
    route(Get, "/ui/fragment/nodes", AnyToken),
    route(Get, "/ui/fragment/alerts", AnyToken),
    route(
        Get,
        "/ui/fragment/app/{app}/{namespace}/instances",
        AnyToken,
    ),
    route(Get, "/ui/app/{app}/{namespace}/env", AnyToken),
    // Workload lifecycle.
    route(Post, "/v1/apply", Deployer),
    route(Get, "/v1/status", AnyToken),
    route(Get, "/v1/apps", AnyToken),
    route(Get, "/v1/readiness", AnyToken),
    route(Get, "/v1/jobs", AnyToken),
    route(Get, "/v1/events", AnyToken),
    route(Get, "/v1/ws/events", AnyToken),
    route(Get, "/v1/ws/logs/{app}/{namespace}", AnyToken),
    route(Get, "/v1/status/{app}/{namespace}", AnyToken),
    route(Get, "/v1/top", AnyToken),
    route(Post, "/v1/stop/{app}/{namespace}", Deployer),
    route(Post, "/v1/delete/{app}/{namespace}", Deployer),
    route(Get, "/v1/logs/{app}/{namespace}", AnyToken),
    route(Get, "/v1/logs/entries/{app}/{namespace}", AnyToken),
    route(Get, "/v1/logs/query/{app}/{namespace}", AnyToken),
    route(Post, "/v1/exec/{app}/{namespace}", Deployer),
    // Cluster + upgrade.
    // Renewal additionally requires the existing node TLS peer certificate.
    route(Post, "/v1/cluster/renew", System),
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
    route(Post, "/v1/upgrade/apply", Admin),
    route(Get, "/v1/upgrade/status", AnyToken),
    route(Post, "/v1/upgrade/rollback", Admin),
    route(Post, "/v1/upgrade/start", Admin),
    route(Get, "/v1/upgrade/cluster", AnyToken),
    route(Post, "/v1/upgrade/resume", Admin),
    route(Post, "/v1/upgrade/abort", Admin),
    route(Post, "/v1/upgrade/cluster-rollback", Admin),
    route(Post, "/v1/cluster/elect", Admin),
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
    route(Get, "/v1/metrics", AnyToken),
    route(Get, "/v1/metrics/summary", AnyToken),
    route(Get, "/v1/metrics/keys", AnyToken),
    route(Get, "/v1/metrics/rollup", AnyToken),
    route(Get, "/v1/metrics/rollup/owned", AnyToken),
    route(Get, "/v1/metrics/cluster", AnyToken),
    route(Get, "/v1/metrics/app/{app}/{namespace}", AnyToken),
    route(Get, "/v1/metrics/app/{app}/{namespace}/chart", AnyToken),
    route(Get, "/v1/alerts", AnyToken),
    route(Get, "/v1/logs/sql", AnyToken),
    route(Post, "/v1/logs/export", Admin),
    route(Get, "/v1/deploys/active", AnyToken),
    route(Get, "/v1/deploys/operations", AnyToken),
    route(Post, "/v1/deploys/operations/{id}/cancel", Deployer),
    route(Get, "/v1/deploys/history/{app}", AnyToken),
    route(Post, "/v1/rollback/{app}/{namespace}", Deployer),
    route(Get, "/v1/placements/{node_id}", AnyToken),
    route(Post, "/v1/test/leases/retired", System),
    route(Post, "/v1/nodes/decommission", Admin),
    route(Get, "/v1/images", AnyToken),
    // Batch + build. `run`/`report`/`track` are node-to-node (System).
    route(Post, "/v1/batch", Deployer),
    route(Post, "/v1/batch/run", System),
    route(Post, "/v1/batch/{id}/report", System),
    route(Get, "/v1/batch/{id}", AnyToken),
    route(Post, "/v1/build", Deployer),
    route(Post, "/v1/build/run", System),
    route(Post, "/v1/build/track", System),
    route(Get, "/v1/build/{id}", AnyToken),
    // GitOps + identity + tokens + secrets.
    route(Post, "/v1/gitops/webhook", AnyToken),
    // Operator-only (Admin). The service principal is refused here (AUTH4),
    // so despite being a signing route it isn't a node-to-node one.
    route(Post, "/v1/identity/sign", Admin),
    // Credential and trust management additionally requires an unscoped user.
    route(Post, "/v1/token/create", Admin),
    route(Get, "/v1/token/list", Admin),
    route(Post, "/v1/token/revoke", Admin),
    route(Post, "/v1/join-token/create", Admin),
    route(Get, "/v1/secret/public-key", AnyToken),
    route(Post, "/v1/secret/rotate", Admin),
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
            "/v1/batch/run",
            "/v1/batch/{id}/report",
            "/v1/build/run",
            "/v1/build/track",
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
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
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
                    let Some(body) = handler_body(source, &handler) else {
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

    /// `/v1/logs/sql` reads across every tenant and takes no app to scope
    /// against, so it must refuse a scoped token outright.
    #[test]
    fn cluster_wide_log_sql_refuses_scoped_tokens() {
        let source = include_str!("api.rs");
        let body = handler_body(source, "logs_sql_handler").expect("logs_sql_handler");
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
        ] {
            let body = handler_body(source, handler).expect(handler);
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

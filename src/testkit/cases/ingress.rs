//! Ingress cases: host-routing through the Wrapper proxy.
//!
//! Need the ingress proxy bound (`[ingress] enabled`), so they require
//! [`Capability::Ingress`] and skip where it's off.
//!
//! [`Capability::Ingress`]: crate::bun::capabilities::Capability::Ingress

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

const INGRESS_HOST: &str = "rbtest-ingress.example";

fn app_with_ingress(ctx: &TestContext, app: &str) -> String {
    format!(
        "{}\n[app.{app}.ingress]\nhost = \"{INGRESS_HOST}\"\npath = \"/\"\n",
        ctx.container_http_spec(app, 1),
    )
}

/// The host part of the entry node's API address — the proxy shares the IP,
/// on the default ingress HTTP port.
fn proxy_base(ctx: &TestContext) -> String {
    let host = ctx
        .client
        .base_url()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or("127.0.0.1");
    format!("http://{host}:80")
}

async fn proxy_status(ctx: &TestContext, path: &str) -> Result<u16, String> {
    let url = format!("{}{path}", proxy_base(ctx));
    let response = ctx
        .client
        .http()
        .get(&url)
        .header("Host", INGRESS_HOST)
        .send()
        .await
        .map_err(|error| format!("request to ingress proxy failed: {error}"))?;
    Ok(response.status().as_u16())
}

/// A request carrying the app's host header reaches the app through the proxy.
async fn ingress_routes_host_header_to_app(ctx: TestContext) -> Result<(), String> {
    let app = "ing-web";
    ctx.apply(&app_with_ingress(&ctx, app)).await?;
    ctx.wait_running_cluster(app, 1).await?;

    let status = proxy_status(&ctx, "/hostname").await?;
    if status != 200 {
        return Err(format!("ingress returned {status}, expected 200"));
    }
    Ok(())
}

/// With no backends, the proxy returns a gateway error rather than a success.
async fn ingress_returns_502_when_no_backends(ctx: TestContext) -> Result<(), String> {
    let app = "ing-gone";
    ctx.apply(&app_with_ingress(&ctx, app)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    ctx.client
        .stop(app, &ctx.namespace)
        .await
        .map_err(|error| format!("stop failed: {error}"))?;

    let status = proxy_status(&ctx, "/hostname").await?;
    if !(500..=599).contains(&status) {
        return Err(format!("expected a 5xx with no backends, got {status}"));
    }
    Ok(())
}

/// The route appears in the routing table with its backend.
async fn ingress_route_appears_in_routing_table(ctx: TestContext) -> Result<(), String> {
    let app = "ing-listed";
    ctx.apply(&app_with_ingress(&ctx, app)).await?;
    ctx.wait_running_cluster(app, 1).await?;

    let routes = ctx
        .client
        .routes()
        .await
        .map_err(|error| format!("could not read routes: {error}"))?;
    let listed = routes
        .iter()
        .any(|route| format!("{route:?}").contains(INGRESS_HOST));
    if !listed {
        return Err(format!("route for {INGRESS_HOST} not in the routing table"));
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "ingress_routes_host_header_to_app",
            group: TestGroup::Ingress,
            requires: &[Capability::ContainerRuntime, Capability::Ingress],
            run: testkit_case!(ingress_routes_host_header_to_app),
        },
        TestCase {
            name: "ingress_returns_502_when_no_backends",
            group: TestGroup::Ingress,
            requires: &[Capability::ContainerRuntime, Capability::Ingress],
            run: testkit_case!(ingress_returns_502_when_no_backends),
        },
        TestCase {
            name: "ingress_route_appears_in_routing_table",
            group: TestGroup::Ingress,
            requires: &[Capability::ContainerRuntime, Capability::Ingress],
            run: testkit_case!(ingress_route_appears_in_routing_table),
        },
    ]
}

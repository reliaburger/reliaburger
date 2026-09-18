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

fn ingress_host(ctx: &TestContext) -> String {
    format!("{}.rbtest-ingress.example", ctx.namespace)
}

fn app_with_ingress(ctx: &TestContext, app: &str) -> Result<String, String> {
    let tls = if ctx.ingress_endpoint()?.scheme() == "https" {
        if ctx.capabilities.has(Capability::IngressClusterTls) {
            "cluster"
        } else {
            "explicit"
        }
    } else {
        "none"
    };
    Ok(format!(
        "{}\n[app.{app}.ingress]\nhost = \"{host}\"\npath = \"/\"\ntls = \"{tls}\"\n",
        ctx.container_http_spec(app, 1),
        host = ingress_host(ctx),
    ))
}

/// Probe the declared listener without forwarding API credentials.
async fn wait_for_proxy(ctx: &TestContext, expected_status: u16) -> Result<(), String> {
    let (mut url, client) = ctx.ingress_probe(&ingress_host(ctx)).await?;
    url.set_path("/hostname");
    let mut last = "no response".to_string();
    let outcome = ctx
        .deadline
        .run("ingress convergence", async {
            loop {
                match client.get(url.clone()).send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        match response.text().await {
                            Ok(body)
                                if status == expected_status
                                    && (status != 200 || body == "reliaburger-test") =>
                            {
                                return;
                            }
                            Ok(_) => last = format!("HTTP {status} or unexpected body"),
                            Err(error) => last = format!("response body: {error}"),
                        }
                    }
                    Err(error) => last = error.to_string(),
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;
    outcome.map_err(|error| format!("{error}; last ingress observation: {last}"))
}

/// A request carrying the app's host header reaches the app through the proxy.
async fn ingress_routes_host_header_to_app(ctx: TestContext) -> Result<(), String> {
    let app = "ing-web";
    ctx.apply(&app_with_ingress(&ctx, app)?).await?;
    ctx.wait_running_cluster(app, 1).await?;

    wait_for_proxy(&ctx, 200).await
}

/// Stopping the app removes its desired route and eventually returns not found.
async fn ingress_removes_route_after_stop(ctx: TestContext) -> Result<(), String> {
    let app = "ing-gone";
    ctx.apply(&app_with_ingress(&ctx, app)?).await?;
    ctx.wait_running_cluster(app, 1).await?;
    wait_for_proxy(&ctx, 200).await?;
    ctx.client
        .stop(app, &ctx.namespace)
        .await
        .map_err(|error| format!("stop failed: {error}"))?;

    wait_for_proxy(&ctx, 404).await
}

/// The route appears in the routing table with its backend.
async fn ingress_route_appears_in_routing_table(ctx: TestContext) -> Result<(), String> {
    let app = "ing-listed";
    ctx.apply(&app_with_ingress(&ctx, app)?).await?;
    ctx.wait_running_cluster(app, 1).await?;

    ctx.deadline
        .run("ingress route convergence", async {
            loop {
                let routes = ctx
                    .client
                    .routes()
                    .await
                    .map_err(|error| format!("could not read routes: {error}"))?;
                if routes
                    .iter()
                    .any(|route| route.host == ingress_host(&ctx) && route.healthy_backends > 0)
                {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|error| error.to_string())?
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
            name: "ingress_removes_route_after_stop",
            group: TestGroup::Ingress,
            requires: &[Capability::ContainerRuntime, Capability::Ingress],
            run: testkit_case!(ingress_removes_route_after_stop),
        },
        TestCase {
            name: "ingress_route_appears_in_routing_table",
            group: TestGroup::Ingress,
            requires: &[Capability::ContainerRuntime, Capability::Ingress],
            run: testkit_case!(ingress_route_appears_in_routing_table),
        },
    ]
}

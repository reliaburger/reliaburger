//! Workload-identity cases: JWKS and token scoping.
//!
//! These exercise the identity/auth control plane, which is API-level and
//! runtime-independent, so they run on a process-runtime cluster too.

use crate::bun::capabilities::Capability;
use crate::config::Config;
use crate::relish::client::BunClient;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A running workload's SPIFFE certificate isn't reachable through the API, so
/// this can't be asserted end-to-end from the harness yet.
async fn workload_receives_spiffe_certificate(_ctx: TestContext) -> Result<(), String> {
    unknown("a workload's SPIFFE certificate is not exposed via the orchestrator API")
}

/// The JWKS endpoint serves at least one well-formed signing key.
async fn jwks_endpoint_serves_signing_keys(ctx: TestContext) -> Result<(), String> {
    // JWKS is a public endpoint (the auth split router leaves it open), so a
    // raw GET without a bearer is correct.
    let url = format!("{}/v1/identity/jwks", ctx.client.base_url());
    let response = ctx
        .client
        .http()
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("jwks request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("jwks returned HTTP {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("jwks response was not JSON: {error}"))?;
    let keys = body["keys"]
        .as_array()
        .ok_or_else(|| "jwks response has no keys array".to_string())?;
    if keys.is_empty() {
        return Err("jwks served no keys".to_string());
    }
    if keys[0].get("kty").is_none() {
        return Err("the first JWK is missing its kty".to_string());
    }
    Ok(())
}

/// A token scoped to one namespace is refused when it writes to another.
async fn namespace_scoped_token_is_rejected_elsewhere(ctx: TestContext) -> Result<(), String> {
    // Mint a Deployer token confined to this test's namespace. (This needs the
    // harness itself to hold an admin token, which the dev cluster provides.)
    let token = ctx
        .client
        .token_create(
            &format!("rbtest-scope-{}", ctx.namespace),
            "Deployer",
            None,
            Some(vec![ctx.namespace.clone()]),
            Some(1),
        )
        .await
        .map_err(|error| format!("could not mint a scoped token: {error}"))?;

    let scoped = BunClient::new_with_token(ctx.client.base_url(), Some(&token));
    let other_namespace = format!("{}-elsewhere", ctx.namespace);
    let spec = format!(
        "[app.probe]\n\
         image = \"proc-grill:image-ignored\"\n\
         command = [\"true\"]\n\
         namespace = \"{other_namespace}\"\n",
    );
    let config =
        Config::parse(&spec).map_err(|error| format!("probe spec does not parse: {error}"))?;

    match scoped.apply(&config).await {
        Err(_) => Ok(()),
        Ok(_) => {
            // It was wrongly allowed — clean up the leak, then fail.
            let _ = ctx.client.stop("probe", &other_namespace).await;
            Err("a namespace-scoped token was allowed to write to another namespace".to_string())
        }
    }
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "workload_receives_spiffe_certificate",
            group: TestGroup::WorkloadIdentity,
            requires: &[Capability::Identity],
            run: testkit_case!(workload_receives_spiffe_certificate),
        },
        TestCase {
            name: "jwks_endpoint_serves_signing_keys",
            group: TestGroup::WorkloadIdentity,
            requires: &[Capability::Council, Capability::Identity],
            run: testkit_case!(jwks_endpoint_serves_signing_keys),
        },
        TestCase {
            name: "namespace_scoped_token_is_rejected_elsewhere",
            group: TestGroup::WorkloadIdentity,
            requires: &[Capability::Identity],
            run: testkit_case!(namespace_scoped_token_is_rejected_elsewhere),
        },
    ]
}

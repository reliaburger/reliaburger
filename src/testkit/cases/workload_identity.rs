//! Workload-identity cases: JWKS and token scoping.
//!
//! These exercise the identity/auth control plane, which is API-level and
//! runtime-independent, so they run on a process-runtime cluster too.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A running workload's SPIFFE certificate isn't reachable through the API, so
/// this can't be asserted end-to-end from the harness yet.
async fn workload_receives_spiffe_certificate(
    _ctx: TestContext,
) -> crate::testkit::registry::CaseResult {
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
        .map_err(|error| error.to_string())?
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

/// A token scoped to one namespace is refused when it reads another's logs.
async fn namespace_scoped_token_is_rejected_elsewhere(
    ctx: TestContext,
) -> crate::testkit::registry::CaseResult {
    let lease_id = ctx
        .lease_id
        .as_deref()
        .ok_or_else(|| "scoped-token probe requires a server-owned lease".to_string())?;
    let token_name = format!("{}-scope", ctx.namespace);
    let token = ctx
        .client
        .token_create_with_lease(&token_name, &ctx.namespace, lease_id)
        .await
        .map_err(|error| format!("could not mint a leased scoped token: {error}"))?;
    let scoped = ctx.client.with_token(&token);
    let other_namespace = format!("outside-{}", ctx.namespace.trim_start_matches("rbtest-"));
    // A read proves the scope gate without creating an unowned resource if that
    // very gate is broken. Raft owns token cleanup even if this future is dropped.
    loop {
        let result = ctx
            .deadline
            .run(
                "scoped-token probe",
                scoped.log_entries("probe", &other_namespace, 1, 0),
            )
            .await
            .map_err(|error| format!("scope enforcement unproven: {error}"))?;
        match result {
            Err(crate::relish::RelishError::ApiError { status: 403, body })
                if body.contains("token scope does not allow") =>
            {
                return Ok(());
            }
            Err(crate::relish::RelishError::ApiError { status: 401, .. }) => {
                // Each node refreshes its local authentication store from Raft.
                ctx.deadline
                    .run(
                        "token propagation",
                        tokio::time::sleep(std::time::Duration::from_millis(100)),
                    )
                    .await
                    .map_err(|error| format!("scope enforcement unproven: {error}"))?;
            }
            Err(
                error @ (crate::relish::RelishError::AgentUnreachable
                | crate::relish::RelishError::RequestTimeout),
            ) => {
                return unknown(format!(
                    "could not probe the scope boundary: {error}; enforcement unproven"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "expected the scope refusal (403 with token scope does not allow), got: {error}"
                )
                .into());
            }
            Ok(_) => {
                return Err(
                    "a namespace-scoped token could read another namespace's logs"
                        .to_string()
                        .into(),
                );
            }
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
            requires: &[Capability::Council, Capability::Identity],
            run: testkit_case!(namespace_scoped_token_is_rejected_elsewhere),
        },
    ]
}

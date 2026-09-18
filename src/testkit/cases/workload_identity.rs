//! Workload-identity cases: JWKS and token scoping.
//!
//! JWKS and token scoping exercise the control plane on any runtime. The
//! certificate case also requires a container and inspects its public bundle.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A workload receives the expected SPIFFE leaf under the configured cluster CA.
async fn workload_receives_spiffe_certificate(
    ctx: TestContext,
) -> crate::testkit::registry::CaseResult {
    use rustls::pki_types::{CertificateDer, UnixTime, pem::PemObject};
    use std::sync::Arc;
    use x509_parser::prelude::{FromDer, X509Certificate};

    let Some(ca_pem) = ctx.client.cluster_ca_pem() else {
        return unknown("certificate verification requires an explicit cluster CA");
    };
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(certificate.map_err(|error| format!("invalid cluster CA PEM: {error}"))?)
            .map_err(|error| format!("invalid cluster CA certificate: {error}"))?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()
    .map_err(|error| format!("cannot configure workload certificate verification: {error}"))?;

    let app = "identity-app";
    ctx.apply(&ctx.container_idle_spec(app)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    let bundle = ctx
        .deadline
        .run("wait for workload certificate", async {
            loop {
                if let Ok(bundle) = ctx
                    .exec_in_workload(
                        app,
                        &[
                            "/bin/busybox".into(),
                            "cat".into(),
                            "/run/reliaburger/identity/bundle.pem".into(),
                        ],
                    )
                    .await
                    && bundle.starts_with("-----BEGIN CERTIFICATE-----")
                {
                    return bundle;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|error| error.to_string())?;
    let certificates = CertificateDer::pem_slice_iter(bundle.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("invalid workload certificate PEM: {error}"))?;
    let (leaf, intermediates) = certificates
        .split_first()
        .ok_or_else(|| "workload certificate bundle is empty".to_string())?;
    verifier
        .verify_client_cert(leaf, intermediates, UnixTime::now())
        .map_err(|error| format!("workload certificate chain is invalid: {error}"))?;

    let (_, certificate) = X509Certificate::from_der(leaf.as_ref())
        .map_err(|error| format!("invalid workload certificate: {error}"))?;
    let expected = crate::sesame::types::SpiffeUri {
        trust_domain: ctx.capabilities.cluster_name.clone(),
        namespace: ctx.namespace.clone(),
        workload_type: crate::sesame::types::WorkloadType::App,
        name: app.into(),
    }
    .to_uri();
    let names = certificate
        .subject_alternative_name()
        .map_err(|error| format!("invalid workload certificate names: {error}"))?
        .ok_or_else(|| "workload certificate has no subject alternative names".to_string())?;
    if names.value.general_names.as_slice()
        != [x509_parser::extensions::GeneralName::URI(&expected)]
    {
        return Err(format!("workload certificate does not identify exactly {expected}").into());
    }
    Ok(())
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
            requires: &[
                Capability::ContainerRuntime,
                Capability::Council,
                Capability::Identity,
            ],
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

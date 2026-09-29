//! Non-default cluster trust-domain acceptance.

use reliaburger::config::node::NodeConfig;
use reliaburger::sesame::identity::CertUsage;
use reliaburger::sesame::types::{SerialNumber, WorkloadType};

#[test]
fn configured_cluster_name_survives_workload_issuance_and_verification() {
    let config = NodeConfig::parse(
        r#"
        [cluster]
        name = "payments.prod"
        "#,
    )
    .unwrap();
    let uri = reliaburger::bun::agent::workload_spiffe_uri(
        &config.cluster.name,
        "checkout",
        "api",
        WorkloadType::App,
    );
    assert_eq!(uri.to_uri(), "spiffe://payments.prod/ns/checkout/app/api");

    let wrapping_ikm = b"test-wrapping-material-32bytes!!";
    let hierarchy =
        reliaburger::sesame::ca::generate_ca_hierarchy(&config.cluster.name, wrapping_ikm).unwrap();
    let (csr_der, _private_key_der) =
        reliaburger::sesame::identity::create_workload_csr(&uri).unwrap();
    let cert_der = reliaburger::sesame::identity::validate_and_sign_csr(
        &csr_der,
        &uri,
        SerialNumber(42),
        CertUsage::Mtls,
        &hierarchy.workload.signing_keypair,
        &hierarchy.workload.certificate_params,
        std::time::SystemTime::now(),
    )
    .unwrap();

    reliaburger::sesame::cert::validate_chain(
        &cert_der,
        &hierarchy.workload.ca.certificate_der,
        &hierarchy.root.ca.certificate_der,
    )
    .unwrap();
    let sans = reliaburger::sesame::cert::subject_uri_sans(&cert_der).unwrap();
    assert_eq!(sans, vec![uri.to_uri()]);
    assert!(!sans.iter().any(|san| san.starts_with("spiffe://default/")));
}

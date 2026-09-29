//! Node issuance must use the real issuer's signed validity window.

use reliaburger::sesame::{
    ca, join,
    types::{SecurityState, SerialNumber},
};

fn hierarchy_with_node_window(start: i64, end: i64) -> ca::CaHierarchy {
    let mut hierarchy = ca::generate_ca_hierarchy("node-validity", b"test-ikm").unwrap();
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap();
    hierarchy.node.certificate_params.not_before = now + time::Duration::seconds(start);
    hierarchy.node.certificate_params.not_after = now + time::Duration::seconds(end);
    let root = hierarchy
        .root
        .certificate_params
        .clone()
        .self_signed(&hierarchy.root.signing_keypair)
        .unwrap();
    hierarchy.node.ca.certificate_der = hierarchy
        .node
        .certificate_params
        .clone()
        .signed_by(
            &hierarchy.node.signing_keypair,
            &root,
            &hierarchy.root.signing_keypair,
        )
        .unwrap()
        .der()
        .to_vec();
    // Leave the sidecar dates unchanged: only the signed certificate is authoritative.
    hierarchy
}

fn issue_all(hierarchy: &ca::CaHierarchy) -> Vec<Result<Vec<u8>, String>> {
    let (csr, _) = ca::create_node_csr("node").unwrap();
    let state = SecurityState {
        certificate_authorities: vec![hierarchy.root.ca.clone(), hierarchy.node.ca.clone()],
        ..Default::default()
    };
    vec![
        ca::issue_node_cert(
            "node",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .map(|(cert, _, _)| cert)
        .map_err(|error| error.to_string()),
        ca::sign_node_csr(
            &csr,
            "node",
            SerialNumber(11),
            ca::NODE_LEAF_LIFETIME,
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .map(|(cert, _)| cert)
        .map_err(|error| error.to_string()),
        join::sign_join_csr(
            &csr,
            "node",
            SerialNumber(12),
            ca::NODE_LEAF_LIFETIME,
            &state,
            b"test-ikm",
        )
        .map(|result| result.certificate_der)
        .map_err(|error| error.to_string()),
    ]
}

#[test]
fn node_issuance_never_outlives_the_real_node_ca() {
    let hierarchy = hierarchy_with_node_window(-5, 90);
    let (_, issuer) =
        x509_parser::parse_x509_certificate(&hierarchy.node.ca.certificate_der).unwrap();
    for (path, result) in issue_all(&hierarchy).into_iter().enumerate() {
        let der = result.unwrap();
        let (_, leaf) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert!(
            leaf.validity().not_before >= issuer.validity().not_before,
            "issuance path {path}"
        );
        assert_eq!(
            leaf.validity().not_after,
            issuer.validity().not_after,
            "issuance path {path}"
        );
        reliaburger::sesame::cert::validate_chain(
            &der,
            &hierarchy.node.ca.certificate_der,
            &hierarchy.root.ca.certificate_der,
        )
        .unwrap();
    }
}

#[test]
fn node_issuance_refuses_expired_and_future_node_cas() {
    for (start, end) in [(-90, -5), (30, 90)] {
        let hierarchy = hierarchy_with_node_window(start, end);
        for (path, result) in issue_all(&hierarchy).into_iter().enumerate() {
            assert!(
                result.is_err(),
                "issuance path {path} accepted invalid issuer window {start}..{end}"
            );
        }
    }
}

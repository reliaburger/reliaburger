//! Chains the cluster issues pass a strict X.509 verifier (#1497).
//!
//! rustls and webpki never look at the authority key identifier, so every
//! other TLS test here passes without one. OpenSSL's `-x509_strict` is the
//! check Python 3.13 turns on by default, and `openssl` needs no new crate.
//! Another `openssl` (macOS ships LibreSSL), or none at all, proves nothing, so
//! each test first shows the tool refuses a chain with the extension missing,
//! and otherwise says so and passes: the suite doesn't depend on it. The unit tests in `sesame::ca` compare the identifiers
//! on every host.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::cert::der_to_pem;
use reliaburger::sesame::identity::{CertUsage, create_workload_csr, validate_and_sign_csr};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config};
use reliaburger::sesame::types::{SerialNumber, SpiffeUri};

fn write_pem(directory: &Path, name: &str, der: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, der_to_pem(der, "CERTIFICATE")).unwrap();
    path
}

fn verify_strictly(root: &[u8], intermediate: &[u8], leaf: &[u8]) -> std::io::Result<Output> {
    let directory = tempfile::tempdir().unwrap();
    let root = write_pem(directory.path(), "root.pem", root);
    let intermediate = write_pem(directory.path(), "intermediate.pem", intermediate);
    let leaf = write_pem(directory.path(), "leaf.pem", leaf);
    Command::new("openssl")
        .arg("verify")
        .arg("-x509_strict")
        .arg("-CAfile")
        .arg(root)
        .arg("-untrusted")
        .arg(intermediate)
        .arg(leaf)
        .output()
}

/// Whether this host's `openssl` refuses a leaf with no authority key
/// identifier, which is the only reason to believe it when it accepts one.
fn openssl_is_strict(hierarchy: &CaHierarchy) -> bool {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["control.example".to_string()]).unwrap();
    params.is_ca = rcgen::IsCa::NoCa;
    let issuer = rcgen::Issuer::from_params(
        &hierarchy.node.certificate_params,
        &hierarchy.node.signing_keypair,
    );
    let control = params.signed_by(&key, &issuer).unwrap();
    let refused = verify_strictly(
        &hierarchy.root.ca.certificate_der,
        &hierarchy.node.ca.certificate_der,
        control.der(),
    )
    .is_ok_and(|output| {
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("authority key identifier")
    });
    if !refused {
        eprintln!("no strict `openssl` on this host; relying on the key identifier unit tests");
    }
    refused
}

#[test]
fn every_kind_of_issued_chain_passes_a_strict_verifier() {
    let ikm = b"strict-ikm";
    let hierarchy = ca::generate_ca_hierarchy("strict", ikm).unwrap();
    if !openssl_is_strict(&hierarchy) {
        return;
    }
    let root = &hierarchy.root.ca.certificate_der;

    // The material a restarted node rebuilds from what it stored.
    let (node_key, node_params) = ca::ca_signing_material(&hierarchy.node.ca, ikm).unwrap();
    let (node_leaf, _, _) =
        ca::issue_node_cert("node-01", SerialNumber(10), &node_key, &node_params).unwrap();

    let (ingress_key, ingress_params) =
        ca::ca_signing_material(&hierarchy.ingress.ca, ikm).unwrap();
    let (ingress_chain, _) = reliaburger::wrapper::tls::issue_ingress_cert(
        &["app.example.com".to_string()],
        Duration::from_secs(3600),
        &ingress_key,
        &ingress_params,
        &rustls::pki_types::CertificateDer::from(hierarchy.ingress.ca.certificate_der.clone()),
    )
    .unwrap();
    let ingress_leaf = ingress_chain[0].to_vec();

    let (workload_key, workload_params) =
        ca::ca_signing_material(&hierarchy.workload.ca, ikm).unwrap();
    let uri = SpiffeUri {
        namespace: "default".to_string(),
        workload_type: reliaburger::sesame::types::WorkloadType::App,
        name: "web".to_string(),
        trust_domain: "strict".to_string(),
    };
    let (csr, _) = create_workload_csr(&uri).unwrap();
    let workload_leaf = validate_and_sign_csr(
        &csr,
        &uri,
        SerialNumber(11),
        CertUsage::Mtls,
        &workload_key,
        &workload_params,
        SystemTime::now(),
    )
    .unwrap();

    for (name, intermediate, leaf) in [
        ("node", &hierarchy.node.ca.certificate_der, &node_leaf),
        (
            "ingress",
            &hierarchy.ingress.ca.certificate_der,
            &ingress_leaf,
        ),
        (
            "workload",
            &hierarchy.workload.ca.certificate_der,
            &workload_leaf,
        ),
    ] {
        let output = verify_strictly(root, intermediate, leaf).unwrap();
        assert!(
            output.status.success(),
            "strict verifier refused the {name} chain: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// Run `openssl s_client` the way a strict client reaches the API: trusting
/// only the cluster root, so the listener has to send the Node CA too. The
/// node certificate is there because this listener asks for a client identity.
fn connect_strictly(address: std::net::SocketAddr, identity: &NodeIdentity) -> String {
    let directory = tempfile::tempdir().unwrap();
    let root = write_pem(directory.path(), "root.pem", &identity.root_ca_der);
    let certificate = write_pem(directory.path(), "client.pem", &identity.certificate_der);
    let key = directory.path().join("client.key");
    std::fs::write(&key, der_to_pem(&identity.private_key_der, "PRIVATE KEY")).unwrap();
    let output = Command::new("openssl")
        .arg("s_client")
        .arg("-connect")
        .arg(address.to_string())
        .arg("-CAfile")
        .arg(root)
        .arg("-cert")
        .arg(certificate)
        .arg("-key")
        .arg(key)
        .arg("-x509_strict")
        .arg("-verify_return_error")
        .arg("-brief")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_strict_client_accepts_the_api_listeners_certificate() {
    let hierarchy = ca::generate_ca_hierarchy("strict", b"strict-ikm").unwrap();
    if !openssl_is_strict(&hierarchy) {
        return;
    }
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        "node-01",
        SerialNumber(10),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    let identity = NodeIdentity {
        node_id: "node-01".to_string(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        trust: reliaburger::sesame::trust::TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ),
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(3600),
    };

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&identity, CrlHandle::default()).unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // `s_client` decides on the chain; the stream's outcome doesn't matter.
        let _ = acceptor.accept(tcp).await;
    });

    let transcript = tokio::task::spawn_blocking(move || connect_strictly(address, &identity))
        .await
        .unwrap();
    server.abort();
    assert!(
        transcript.contains("Verification: OK"),
        "strict client refused the API listener: {transcript}"
    );
}

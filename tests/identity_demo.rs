//! Workload identity demo — exercises the full SPIFFE certificate lifecycle.
//!
//! Run with: cargo test --test identity_demo -- --nocapture

use reliaburger::sesame::types::{CaRole, SpiffeUri, WorkloadJwtClaims, WorkloadType};
use reliaburger::sesame::{ca, cert, identity, init, oidc};
use std::time::SystemTime;

fn section(title: &str) {
    println!("\n\x1b[1;36m=== {title} ===\x1b[0m\n");
}

fn step(msg: &str) {
    println!("\x1b[32m--- {msg} ---\x1b[0m");
}

#[test]
fn demo_cluster_init_and_ca_hierarchy() {
    section("1. Cluster Initialisation");
    step("generating CA hierarchy + OIDC keypair");

    let dir = tempfile::tempdir().unwrap();
    let result = init::initialize_cluster("demo-cluster", "node-01", dir.path()).unwrap();

    let state = &result.security_state;
    let root = state.get_ca(CaRole::Root).unwrap();
    let workload_ca = state.get_ca(CaRole::Workload).unwrap();

    println!("  Cluster:     demo-cluster");
    println!("  Root CA:     serial {}", root.serial);
    println!("  Workload CA: serial {}", workload_ca.serial);

    let oidc = state.oidc_signing_config.as_ref().unwrap();
    println!("  OIDC key ID: {}", oidc.key_id);
    println!("  OIDC issuer: {}", oidc.issuer);
    println!("  Ed25519 public key: {} bytes", oidc.public_key_der.len());

    assert_eq!(state.certificate_authorities.len(), 4);
    assert!(state.oidc_signing_config.is_some());
}

#[test]
fn demo_csr_generation_and_signing() {
    section("2. CSR Generation + Signing");

    let dir = tempfile::tempdir().unwrap();
    let result = init::initialize_cluster("demo-cluster", "node-01", dir.path()).unwrap();
    let wrapping_ikm = b"test-wrapping-material-32bytes!";
    let hierarchy = ca::generate_ca_hierarchy("demo-cluster", wrapping_ikm).unwrap();

    let spiffe_uri = SpiffeUri {
        trust_domain: "demo-cluster".to_string(),
        namespace: "production".to_string(),
        workload_type: WorkloadType::App,
        name: "api".to_string(),
    };

    step("worker generates keypair + CSR");
    let (csr_der, private_key_der) = identity::create_workload_csr(&spiffe_uri).unwrap();
    println!("  SPIFFE URI:  {}", spiffe_uri.to_uri());
    println!("  CSR size:    {} bytes", csr_der.len());
    println!(
        "  Private key: {} bytes (stays on worker)",
        private_key_der.len()
    );

    step("council validates + signs CSR with Workload CA");
    let serial = result.security_state.next_serial;
    let cert_der = identity::validate_and_sign_csr(
        &csr_der,
        &spiffe_uri,
        reliaburger::sesame::types::SerialNumber(serial),
        &hierarchy.workload.signing_keypair,
        &hierarchy.workload.certificate_params,
    )
    .unwrap();

    let (_, parsed) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
    println!("  Signed cert: {} bytes", cert_der.len());
    println!("  Subject:     {}", parsed.subject());
    println!("  Is CA:       {}", parsed.is_ca());

    step("verifying cert chain: workload -> Workload CA");
    cert::verify_signature(&cert_der, &hierarchy.workload.ca.certificate_der).unwrap();
    println!("  Chain verification: OK");
}

#[test]
fn demo_oidc_jwt() {
    section("3. OIDC JWT Minting + Verification");

    let wrapping_ikm = b"test-wrapping-material-32bytes!";
    let config =
        oidc::generate_oidc_keypair("https://demo-cluster.reliaburger.dev", wrapping_ikm).unwrap();

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = WorkloadJwtClaims {
        iss: "https://demo-cluster.reliaburger.dev".to_string(),
        sub: "spiffe://demo-cluster/ns/production/app/api".to_string(),
        aud: vec!["spiffe://demo-cluster".to_string()],
        exp: now + 3600,
        iat: now,
        namespace: "production".to_string(),
        app: "api".to_string(),
        cluster: "demo-cluster".to_string(),
        node: "node-01".to_string(),
        instance: "api-g1234-0".to_string(),
    };

    step("minting JWT");
    let token = oidc::mint_jwt(&claims, &config, wrapping_ikm).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    println!("  Token parts: {}", parts.len());
    println!("  Header:  {}...", &parts[0][..30.min(parts[0].len())]);
    println!("  Payload: {}...", &parts[1][..50.min(parts[1].len())]);
    println!("  Sig:     {}...", &parts[2][..30.min(parts[2].len())]);

    step("verifying JWT");
    let verified = oidc::verify_jwt(&token, &config).unwrap();
    println!("  Subject: {}", verified.sub);
    println!("  Issuer:  {}", verified.iss);
    println!("  Cluster: {}", verified.cluster);
    println!("  Node:    {}", verified.node);
    assert_eq!(verified, claims);
    println!("  Claims match: OK");

    step("JWKS endpoint response");
    let jwks = oidc::jwks_response(&config);
    println!("{}", serde_json::to_string_pretty(&jwks).unwrap());
}

#[test]
fn demo_identity_bundle_and_tmpfs() {
    section("4. Identity Bundle + Tmpfs Delivery");

    let wrapping_ikm = b"test-wrapping-material-32bytes!";
    let hierarchy = ca::generate_ca_hierarchy("demo-cluster", wrapping_ikm).unwrap();
    let oidc_config =
        oidc::generate_oidc_keypair("https://demo-cluster.reliaburger.dev", wrapping_ikm).unwrap();

    let spiffe_uri = SpiffeUri {
        trust_domain: "demo-cluster".to_string(),
        namespace: "production".to_string(),
        workload_type: WorkloadType::App,
        name: "api".to_string(),
    };

    let (csr_der, private_key_der) = identity::create_workload_csr(&spiffe_uri).unwrap();
    let cert_der = identity::validate_and_sign_csr(
        &csr_der,
        &spiffe_uri,
        reliaburger::sesame::types::SerialNumber(10),
        &hierarchy.workload.signing_keypair,
        &hierarchy.workload.certificate_params,
    )
    .unwrap();

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = WorkloadJwtClaims {
        iss: oidc_config.issuer.clone(),
        sub: spiffe_uri.to_uri(),
        aud: vec![format!("spiffe://demo-cluster")],
        exp: now + 3600,
        iat: now,
        namespace: "production".to_string(),
        app: "api".to_string(),
        cluster: "demo-cluster".to_string(),
        node: "node-01".to_string(),
        instance: "api-g1234-0".to_string(),
    };
    let jwt = oidc::mint_jwt(&claims, &oidc_config, wrapping_ikm).unwrap();

    step("building identity bundle");
    let bundle = identity::build_identity_bundle(
        spiffe_uri,
        cert_der,
        private_key_der,
        &hierarchy.workload.ca.certificate_der,
        &hierarchy.root.ca.certificate_der,
        jwt,
    );
    println!("  SPIFFE URI:  {}", bundle.spiffe_uri);
    println!("  Cert size:   {} bytes", bundle.certificate_der.len());
    println!("  CA chain:    {} bytes", bundle.ca_chain_pem.len());
    println!("  JWT length:  {} chars", bundle.jwt_token.len());

    step("writing to tmpfs");
    let dir = tempfile::tempdir().unwrap();
    identity::write_identity_to_tmpfs(&bundle, dir.path()).unwrap();

    let id_dir = dir.path().join("identity");
    for name in &["cert.pem", "key.pem", "ca.pem", "bundle.pem", "token"] {
        let path = id_dir.join(name);
        let size = std::fs::metadata(&path).unwrap().len();
        println!("  {:<12} {} bytes", name, size);
    }
}

#[test]
fn demo_rotation_state_machine() {
    section("5. Rotation State Machine");

    let spiffe_uri = SpiffeUri {
        trust_domain: "demo-cluster".to_string(),
        namespace: "production".to_string(),
        workload_type: WorkloadType::App,
        name: "api".to_string(),
    };
    let now = SystemTime::now();

    step("fresh identity -> Valid");
    let mut id = reliaburger::sesame::types::WorkloadIdentity {
        spiffe_uri,
        certificate_der: vec![],
        private_key_der: vec![],
        ca_chain_pem: String::new(),
        jwt_token: String::new(),
        issued_at: now,
        expires_at: now + std::time::Duration::from_secs(3600),
        next_rotation: now + std::time::Duration::from_secs(1800),
        grace_extended: false,
    };
    let state = identity::rotation_state(&id, now);
    println!("  State: {:?}", state);
    assert_eq!(state, identity::RotationState::Valid);

    step("after 30 min -> NeedsRotation");
    let future = now + std::time::Duration::from_secs(2000);
    let state = identity::rotation_state(&id, future);
    println!("  State: {:?}", state);
    assert_eq!(state, identity::RotationState::NeedsRotation);

    step("extending grace period");
    let extended = identity::extend_grace_period(&mut id);
    println!("  Extended: {extended}");
    println!("  Grace extended: {}", id.grace_extended);
    let state = identity::rotation_state(&id, future);
    println!("  State: {:?}", state);
    assert_eq!(state, identity::RotationState::GracePeriod);

    step("after 5+ hours -> Expired");
    let far_future = now + std::time::Duration::from_secs(20000);
    let state = identity::rotation_state(&id, far_future);
    println!("  State: {:?}", state);
    assert_eq!(state, identity::RotationState::Expired);
}

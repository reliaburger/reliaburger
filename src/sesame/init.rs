//! Cluster initialisation — generates the full CA hierarchy,
//! age keypair, and first join token.
//!
//! Called by `relish init` to bootstrap a new cluster's security state.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::ca::{self, CaHierarchy};
use super::crypto;
use super::oidc;
use super::secret;
use super::types::{
    AgeKeyScope, AttestationMode, CertificateAuthority, JoinToken, NodeCertificate, SecurityState,
    SerialNumber,
};

/// Errors from cluster initialisation.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("CA generation failed: {0}")]
    CaFailed(#[from] ca::CaError),
    #[error("secret key generation failed: {0}")]
    SecretFailed(#[from] secret::SecretError),
    #[error("crypto error: {0}")]
    CryptoFailed(#[from] crypto::CryptoError),
    #[error("failed to write sealed root CA backup: {0}")]
    IoFailed(#[from] std::io::Error),
    #[error("OIDC keypair generation failed: {0}")]
    OidcFailed(#[from] oidc::OidcError),
}

/// The result of initialising a new cluster.
pub struct InitResult {
    /// The security state to store in Raft.
    pub security_state: SecurityState,
    /// The first node's certificate (private key stays on this node).
    pub node_certificate: NodeCertificate,
    /// The join token plaintext (shown to admin once).
    pub join_token_plaintext: String,
    /// The age public key (for encrypting secrets).
    pub age_public_key: String,
    /// The cluster name.
    pub cluster_name: String,
    /// Path to the sealed root CA backup file.
    pub sealed_root_ca_path: String,
    /// OIDC signing key ID (for JWKS endpoint).
    pub oidc_key_id: String,
    /// Master secret (HKDF IKM) used to wrap CA and OIDC private keys.
    /// Must be persisted to a secure file. NEVER store in Raft.
    pub master_secret: [u8; 32],
}

/// Default join token TTL: 15 minutes.
const JOIN_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// Initialise a new cluster's security state.
///
/// Generates the full CA hierarchy, age keypair, first node certificate,
/// and a one-time join token. The root CA private key is encrypted with
/// the age public key and written to disk as a sealed backup.
pub fn initialize_cluster(
    cluster_name: &str,
    node_id: &str,
    output_dir: &Path,
) -> Result<InitResult, InitError> {
    // Step 1: Generate a master secret for HKDF key wrapping
    let master_secret: [u8; 32] = crypto::random_bytes()?;

    // Step 2: Generate the full CA hierarchy
    let hierarchy: CaHierarchy = ca::generate_ca_hierarchy(cluster_name, &master_secret)?;

    // Step 3: Generate an age keypair for secret encryption
    let (age_keypair, _age_identity) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &master_secret, 0)?;
    let age_public_key = age_keypair.public_key.clone();

    // Step 3b: Generate OIDC Ed25519 signing keypair
    let oidc_issuer = format!("https://{cluster_name}.reliaburger.dev");
    let oidc_config = oidc::generate_oidc_keypair(&oidc_issuer, &master_secret)?;
    let oidc_key_id = oidc_config.key_id.clone();

    // Step 4: Seal the root CA private key with age
    let sealed_root =
        secret::seal_with_age(&hierarchy.root.private_key_der, &age_keypair.public_key)?;
    let sealed_path = output_dir.join(format!("{cluster_name}-root-ca.age"));
    std::fs::write(&sealed_path, &sealed_root)?;

    // Step 5: Issue the first node's certificate
    let node_serial = SerialNumber(5); // Root=1, Node=2, Workload=3, Ingress=4, first node=5
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        node_id,
        node_serial,
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )?;

    let now = SystemTime::now();
    let node_certificate = NodeCertificate {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
        ca_generation: 0,
    };

    // Step 6: Generate a join token
    let (token_plaintext, token_hash) = ca::generate_join_token()?;
    let join_token = JoinToken {
        token_hash,
        expires_at: SystemTime::now() + JOIN_TOKEN_TTL,
        consumed: false,
        attestation_mode: AttestationMode::None,
    };

    // Step 7: Wrap the root CA private key with age and store in the CA
    // (Root CA private key is deleted from cluster nodes — only sealed backup exists)
    let root_ca_wrapped = CertificateAuthority {
        private_key_wrapped: None, // Root key NOT stored in Raft
        ..hierarchy.root.ca
    };

    // Step 8: Build the security state for Raft
    let security_state = SecurityState {
        certificate_authorities: vec![
            root_ca_wrapped,
            hierarchy.node.ca,
            hierarchy.workload.ca,
            hierarchy.ingress.ca,
        ],
        age_keypairs: vec![age_keypair],
        api_tokens: vec![],
        join_tokens: vec![join_token],
        next_serial: 6, // Next available serial after root(1), node(2), workload(3), ingress(4), first-node(5)
        oidc_signing_config: Some(oidc_config),
        crl: super::types::Crl::default(),
    };

    Ok(InitResult {
        security_state,
        node_certificate,
        join_token_plaintext: token_plaintext,
        age_public_key,
        cluster_name: cluster_name.to_string(),
        sealed_root_ca_path: sealed_path.display().to_string(),
        oidc_key_id,
        master_secret,
    })
}

/// Format the init output for display to the admin.
pub fn format_init_output(result: &InitResult) -> String {
    let state = &result.security_state;

    let root = state.get_ca(super::types::CaRole::Root).unwrap();
    let node_ca = state.get_ca(super::types::CaRole::Node).unwrap();
    let workload_ca = state.get_ca(super::types::CaRole::Workload).unwrap();
    let ingress_ca = state.get_ca(super::types::CaRole::Ingress).unwrap();

    format!(
        "Cluster initialised.\n\
         \n\
         \x20 Cluster name:    {cluster}\n\
         \x20 Root CA:         serial {root_serial}\n\
         \x20 Node CA:         serial {node_serial}\n\
         \x20 Workload CA:     serial {workload_serial}\n\
         \x20 Ingress CA:      serial {ingress_serial}\n\
         \n\
         \x20 IMPORTANT: Back up the sealed root CA key:\n\
         \x20   {sealed_path}\n\
         \n\
         \x20 Losing this file means a full PKI re-bootstrap.\n\
         \n\
         \x20 OIDC key ID:     {oidc_kid}\n\
         \n\
         \x20 Join token (valid 15 minutes, single use):\n\
         \x20   {join_token}\n",
        cluster = result.cluster_name,
        root_serial = root.serial,
        node_serial = node_ca.serial,
        workload_serial = workload_ca.serial,
        ingress_serial = ingress_ca.serial,
        sealed_path = result.sealed_root_ca_path,
        oidc_kid = result.oidc_key_id,
        join_token = result.join_token_plaintext,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_cluster_generates_all_four_cas() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("test-cluster", "node-01", dir.path()).unwrap();

        let state = &result.security_state;
        assert_eq!(state.certificate_authorities.len(), 4);

        use super::super::types::CaRole;
        assert!(state.get_ca(CaRole::Root).is_some());
        assert!(state.get_ca(CaRole::Node).is_some());
        assert!(state.get_ca(CaRole::Workload).is_some());
        assert!(state.get_ca(CaRole::Ingress).is_some());
    }

    #[test]
    fn initialize_cluster_root_ca_has_no_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("test", "node-01", dir.path()).unwrap();

        let root = result
            .security_state
            .get_ca(super::super::types::CaRole::Root)
            .unwrap();
        assert!(root.private_key_wrapped.is_none());
    }

    #[test]
    fn initialize_cluster_produces_usable_join_token() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("test", "node-01", dir.path()).unwrap();

        assert!(result.join_token_plaintext.starts_with("rbrg_join_1_"));
        assert_eq!(result.security_state.join_tokens.len(), 1);

        let stored = &result.security_state.join_tokens[0];
        assert!(!stored.consumed);
        assert!(stored.expires_at > SystemTime::now());
        assert!(super::super::ca::verify_join_token(
            &result.join_token_plaintext,
            &stored.token_hash
        ));
    }

    #[test]
    fn initialize_cluster_writes_sealed_root_ca() {
        let dir = tempfile::tempdir().unwrap();
        let _result = initialize_cluster("test", "node-01", dir.path()).unwrap();

        let sealed_path = dir.path().join("test-root-ca.age");
        assert!(sealed_path.exists());
        let sealed_bytes = std::fs::read(&sealed_path).unwrap();
        assert!(!sealed_bytes.is_empty());
    }

    #[test]
    fn initialize_cluster_issues_node_cert() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("test", "node-01", dir.path()).unwrap();

        let cert = &result.node_certificate;
        assert_eq!(cert.node_id, "node-01");
        assert!(!cert.certificate_der.is_empty());
        assert!(!cert.private_key_der.is_empty());

        // Verify the node cert chains to the Node CA
        let node_ca = result
            .security_state
            .get_ca(super::super::types::CaRole::Node)
            .unwrap();
        super::super::cert::verify_signature(&cert.certificate_der, &node_ca.certificate_der)
            .unwrap();
    }

    #[test]
    fn initialize_cluster_generates_age_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("test", "node-01", dir.path()).unwrap();

        assert!(!result.age_public_key.is_empty());
        assert_eq!(result.security_state.age_keypairs.len(), 1);
        assert_eq!(
            result.security_state.age_keypairs[0].scope,
            AgeKeyScope::ClusterWide
        );
    }

    #[test]
    fn format_init_output_contains_key_info() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("prod", "node-01", dir.path()).unwrap();
        let output = format_init_output(&result);

        assert!(output.contains("Cluster initialised."));
        assert!(output.contains("prod"));
        assert!(output.contains("rbrg_join_1_"));
        assert!(output.contains("root-ca.age"));
        assert!(output.contains("OIDC key ID:"));
    }

    #[test]
    fn initialize_cluster_generates_oidc_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("oidc-test", "node-01", dir.path()).unwrap();

        let oidc = result.security_state.oidc_signing_config.as_ref();
        assert!(oidc.is_some(), "OIDC signing config should be present");
        let oidc = oidc.unwrap();
        assert!(!oidc.key_id.is_empty());
        assert_eq!(oidc.key_id.len(), 16); // 8 bytes = 16 hex chars
        assert!(oidc.issuer.contains("oidc-test"));
        assert_eq!(oidc.public_key_der.len(), 32); // Ed25519 public key
        assert_eq!(result.oidc_key_id, oidc.key_id);
    }

    #[test]
    fn initialize_cluster_returns_nonzero_master_secret() {
        let dir = tempfile::tempdir().unwrap();
        let result = initialize_cluster("secret-test", "node-01", dir.path()).unwrap();

        assert_ne!(result.master_secret, [0u8; 32]);
        assert_eq!(result.master_secret.len(), 32);
    }
}

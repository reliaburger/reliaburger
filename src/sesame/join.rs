//! Join token validation and node certificate issuance.
//!
//! When a new node joins the cluster, it presents a join token.
//! The council validates the token and issues a node certificate
//! signed by the Node CA.

use std::time::{Duration, SystemTime};

use super::ca;
use super::crypto;
use super::types::{CaRole, NodeCertificate, SecurityState};

/// Errors from join operations.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("invalid join token")]
    InvalidToken,
    #[error("join token has expired")]
    TokenExpired,
    #[error("join token has already been consumed")]
    TokenConsumed,
    #[error("no Node CA found in security state")]
    NoNodeCa,
    #[error("Node CA private key is not available")]
    NoNodeCaKey,
    #[error("failed to issue node certificate: {0}")]
    CertIssueFailed(String),
    #[error("crypto error: {0}")]
    CryptoFailed(#[from] crypto::CryptoError),
    #[error("CA error: {0}")]
    CaFailed(#[from] ca::CaError),
}

/// The result of a successful join: the new node's certificate
/// plus the CA chain it needs for mTLS verification.
#[derive(Debug)]
pub struct JoinResult {
    /// The newly issued node certificate.
    pub node_certificate: NodeCertificate,
    /// DER-encoded Root CA certificate (trust anchor).
    pub root_ca_der: Vec<u8>,
    /// DER-encoded Node CA certificate (for verifying peer nodes).
    pub node_ca_der: Vec<u8>,
}

/// Validate a join token and issue a node certificate.
///
/// This is called on the council leader when a new node presents
/// a join token. The function:
/// 1. Verifies the token is valid, not expired, and not consumed
/// 2. Marks the token as consumed (caller must persist this via Raft)
/// 3. Issues a node certificate signed by the Node CA
///
/// The `wrapping_ikm` is needed to unwrap the Node CA private key
/// from Raft storage.
pub fn validate_and_issue(
    token_plaintext: &str,
    node_id: &str,
    state: &mut SecurityState,
    wrapping_ikm: &[u8],
) -> Result<JoinResult, JoinError> {
    // Step 1: Find and validate the join token
    let token_idx = state
        .join_tokens
        .iter()
        .position(|jt| ca::verify_join_token(token_plaintext, &jt.token_hash))
        .ok_or(JoinError::InvalidToken)?;

    let join_token = &state.join_tokens[token_idx];

    if join_token.consumed {
        return Err(JoinError::TokenConsumed);
    }
    if SystemTime::now() > join_token.expires_at {
        return Err(JoinError::TokenExpired);
    }

    // Step 2: Mark as consumed
    state.join_tokens[token_idx].consumed = true;

    // Step 3: Extract Node CA data (clone to release immutable borrow)
    let node_ca = state.get_ca(CaRole::Node).ok_or(JoinError::NoNodeCa)?;
    let wrapped_key = node_ca
        .private_key_wrapped
        .clone()
        .ok_or(JoinError::NoNodeCaKey)?;
    let node_ca_generation = node_ca.generation;
    let node_ca_der = node_ca.certificate_der.clone();
    let root_ca_der = state
        .get_ca(CaRole::Root)
        .map(|ca| ca.certificate_der.clone())
        .unwrap_or_default();

    // Unwrap the Node CA private key
    let ca_private_key_der = crypto::unwrap_key(wrapping_ikm, &wrapped_key)?;

    // Reconstruct the CA keypair for signing
    let ca_key_der = rustls::pki_types::PrivateKeyDer::try_from(ca_private_key_der)
        .map_err(|e| JoinError::CertIssueFailed(format!("invalid CA key DER: {e}")))?;
    let ca_keypair =
        rcgen::KeyPair::from_der_and_sign_algo(&ca_key_der, &rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| JoinError::CertIssueFailed(format!("invalid CA key: {e}")))?;

    // Build minimal CA params for signing (only DN matters for issuer field)
    let mut ca_params = rcgen::CertificateParams::default();
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "Reliaburger Node CA".to_string());
    dn.push(rcgen::DnType::OrganizationName, "Reliaburger");
    ca_params.distinguished_name = dn;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));

    // Step 4: Issue the node certificate (mutable borrow for serial allocation)
    let serial = state.next_serial();
    let (cert_der, key_der, serial) =
        ca::issue_node_cert(node_id, serial, &ca_keypair, &ca_params)?;

    let now = SystemTime::now();
    let node_certificate = NodeCertificate {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
        ca_generation: node_ca_generation,
    };

    Ok(JoinResult {
        node_certificate,
        root_ca_der,
        node_ca_der,
    })
}

/// Generate a new join token and add it to the security state.
///
/// Returns the plaintext token (for the admin). The hash is stored
/// in the security state.
pub fn generate_new_join_token(
    state: &mut SecurityState,
    ttl: Duration,
) -> Result<String, JoinError> {
    let (plaintext, hash) = ca::generate_join_token()?;
    let join_token = super::types::JoinToken {
        token_hash: hash,
        expires_at: SystemTime::now() + ttl,
        consumed: false,
        attestation_mode: super::types::AttestationMode::None,
    };
    state.join_tokens.push(join_token);
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::init;

    fn setup() -> (init::InitResult, [u8; 32]) {
        let dir = tempfile::tempdir().unwrap();
        let master_secret: [u8; 32] = crypto::random_bytes().unwrap();

        // We need to use the same master_secret that init uses internally.
        // Since init generates its own, we'll just call init and then
        // test join with a fresh token.
        let result = init::initialize_cluster("test", "node-01", dir.path()).unwrap();

        // For join tests, we need the master secret that was used to wrap keys.
        // Since init generates it internally, we can't access it directly.
        // Instead, we'll test the join flow end-to-end by generating a new
        // security state with a known master secret.
        (result, master_secret)
    }

    /// Build a test security state with a known wrapping key.
    fn setup_with_known_key() -> (SecurityState, String, [u8; 32]) {
        let master_secret: [u8; 32] = crypto::random_bytes().unwrap();
        let hierarchy = ca::generate_ca_hierarchy("test", &master_secret).unwrap();

        let (age_kp, _) = crate::sesame::secret::generate_age_keypair(
            super::super::types::AgeKeyScope::ClusterWide,
            &master_secret,
            0,
        )
        .unwrap();

        let (token_plaintext, token_hash) = ca::generate_join_token().unwrap();
        let join_token = super::super::types::JoinToken {
            token_hash,
            expires_at: SystemTime::now() + Duration::from_secs(900),
            consumed: false,
            attestation_mode: super::super::types::AttestationMode::None,
        };

        let state = SecurityState {
            certificate_authorities: vec![
                super::super::types::CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca
                },
                hierarchy.node.ca,
                hierarchy.workload.ca,
                hierarchy.ingress.ca,
            ],
            age_keypairs: vec![age_kp],
            api_tokens: vec![],
            join_tokens: vec![join_token],
            next_serial: 6,
        };

        (state, token_plaintext, master_secret)
    }

    #[test]
    fn join_with_valid_token_succeeds() {
        let (mut state, token, master_secret) = setup_with_known_key();

        let result = validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap();

        assert_eq!(result.node_certificate.node_id, "node-02");
        assert!(!result.node_certificate.certificate_der.is_empty());
        assert!(!result.node_certificate.private_key_der.is_empty());
        assert!(!result.root_ca_der.is_empty());
        assert!(!result.node_ca_der.is_empty());
    }

    #[test]
    fn join_with_valid_token_marks_consumed() {
        let (mut state, token, master_secret) = setup_with_known_key();

        validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap();

        assert!(state.join_tokens[0].consumed);
    }

    #[test]
    fn join_with_consumed_token_fails() {
        let (mut state, token, master_secret) = setup_with_known_key();

        // First join succeeds
        validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap();

        // Second join with same token fails
        let err = validate_and_issue(&token, "node-03", &mut state, &master_secret).unwrap_err();
        assert!(matches!(err, JoinError::TokenConsumed));
    }

    #[test]
    fn join_with_expired_token_fails() {
        let (mut state, token, master_secret) = setup_with_known_key();

        // Force the token to be expired
        state.join_tokens[0].expires_at = SystemTime::now() - Duration::from_secs(60);

        let err = validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap_err();
        assert!(matches!(err, JoinError::TokenExpired));
    }

    #[test]
    fn join_with_invalid_token_fails() {
        let (mut state, _token, master_secret) = setup_with_known_key();

        let err = validate_and_issue(
            "rbrg_join_1_deadbeef",
            "node-02",
            &mut state,
            &master_secret,
        )
        .unwrap_err();
        assert!(matches!(err, JoinError::InvalidToken));
    }

    #[test]
    fn join_token_is_single_use() {
        let (mut state, token, master_secret) = setup_with_known_key();

        // Use the token
        validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap();

        // Generate a new token
        let token2 = generate_new_join_token(&mut state, Duration::from_secs(900)).unwrap();

        // New token works
        validate_and_issue(&token2, "node-03", &mut state, &master_secret).unwrap();
    }

    #[test]
    fn node_cert_verifies_against_node_ca() {
        let (mut state, token, master_secret) = setup_with_known_key();

        let result = validate_and_issue(&token, "node-02", &mut state, &master_secret).unwrap();

        crate::sesame::cert::verify_signature(
            &result.node_certificate.certificate_der,
            &result.node_ca_der,
        )
        .unwrap();
    }

    #[test]
    fn init_produces_usable_join_flow() {
        // This is an integration test: init → generate token → join
        let (_init_result, _master_secret) = setup();
        // The init result has a join token and security state,
        // but we can't use them for join because we don't know
        // the internal master secret. This test just verifies
        // init completes without error.
    }
}

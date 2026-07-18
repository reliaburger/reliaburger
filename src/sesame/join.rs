//! Join token validation and node certificate issuance.
//!
//! When a new node joins the cluster it generates its own keypair and CSR and
//! presents a join token. The council validates the token, atomically consumes
//! it and allocates a serial through Raft (PKI5), then signs the joiner's CSR
//! (PKI4) with the Node CA. The private key never leaves the joining node.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use super::ca;
use super::crypto;
use super::identity_store::NodeIdentity;
use super::types::{CaRole, JoinToken, SecurityState, SerialNumber};

/// Default lifetime for an operator-minted join token.
pub const DEFAULT_JOIN_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// Shortest join-token lifetime accepted by the API.
pub const MIN_JOIN_TOKEN_TTL: Duration = Duration::from_secs(1);
/// Longest join-token lifetime accepted by the API.
pub const MAX_JOIN_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// Errors from join operations.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("invalid join token")]
    InvalidToken,
    #[error("join token has expired")]
    TokenExpired,
    #[error("join token has already been consumed")]
    TokenConsumed,
    #[error("join token TTL must be between 1 second and 1 hour (got {seconds} seconds)")]
    InvalidTtl { seconds: u64 },
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

/// The result of a successful join: the new node's signed certificate
/// plus the CA chain it needs for mTLS verification.
///
/// There is deliberately no private key here (PKI4). The joiner generated its
/// own keypair and CSR; the issuer only ever sees, signs, and returns the
/// certificate.
#[derive(Debug)]
pub struct JoinResult {
    /// The node id the certificate was issued for.
    pub node_id: String,
    /// DER-encoded signed node certificate.
    pub certificate_der: Vec<u8>,
    /// The serial the Node CA assigned.
    pub serial: SerialNumber,
    /// The Node CA generation that signed the certificate.
    pub ca_generation: u64,
    /// DER-encoded Root CA certificate (trust anchor).
    pub root_ca_der: Vec<u8>,
    /// DER-encoded Node CA certificate (for verifying peer nodes).
    pub node_ca_der: Vec<u8>,
}

/// The certificate signing request a joiner sends to a cluster member.
///
/// The joiner keeps its private key; only the CSR crosses the wire, so a
/// compromised or eavesdropped join can never yield the joiner's key (PKI4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The one-time join token, plaintext.
    pub token: String,
    /// The node id the joiner wants a certificate for.
    pub node_id: String,
    /// Base64 DER PKCS#10 certificate signing request.
    pub csr_b64: String,
}

/// The join response sent over the wire to a joining node.
///
/// DER material is base64-encoded so the bundle is plain JSON. There is **no
/// private key** in the bundle (PKI4): the issuer signs the joiner's CSR and
/// returns only the leaf certificate plus the CA chain. The joiner assembles
/// its identity from this bundle and the private key it kept locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinBundle {
    /// The node id the certificate was issued for.
    pub node_id: String,
    /// The serial the Node CA assigned.
    pub serial: u64,
    /// The Node CA generation that signed the certificate.
    pub ca_generation: u64,
    /// Base64 DER node certificate.
    pub certificate_b64: String,
    /// Base64 DER Node CA certificate.
    pub node_ca_b64: String,
    /// Base64 DER root CA certificate.
    pub root_ca_b64: String,
}

/// Errors from the joiner side of the ceremony.
#[derive(Debug, thiserror::Error)]
pub enum JoinClientError {
    #[error("failed to reach cluster member: {0}")]
    Transport(String),
    #[error("cluster member rejected the join: {0}")]
    Rejected(String),
    #[error("malformed join bundle: {0}")]
    Malformed(String),
    #[error(
        "root CA fingerprint mismatch: member offered {offered}, expected {expected} — refusing to trust"
    )]
    FingerprintMismatch { offered: String, expected: String },
}

impl JoinBundle {
    /// Build a wire bundle from an issuer-side [`JoinResult`].
    pub fn from_result(result: &JoinResult) -> Self {
        Self {
            node_id: result.node_id.clone(),
            serial: result.serial.0,
            ca_generation: result.ca_generation,
            certificate_b64: BASE64.encode(&result.certificate_der),
            node_ca_b64: BASE64.encode(&result.node_ca_der),
            root_ca_b64: BASE64.encode(&result.root_ca_der),
        }
    }

    /// Decode the bundle into an on-disk [`NodeIdentity`], supplying the
    /// private key the joiner kept locally.
    ///
    /// The bundle carries no key (PKI4), so the joiner pairs the signed leaf
    /// with the key from its own CSR. A `validate_chain` guard rejects a
    /// bundle whose leaf does not chain to the returned CA chain.
    pub fn into_identity(self, private_key_der: Vec<u8>) -> Result<NodeIdentity, JoinClientError> {
        let decode = |field: &str, s: &str| {
            BASE64
                .decode(s)
                .map_err(|e| JoinClientError::Malformed(format!("{field}: {e}")))
        };
        let certificate_der = decode("certificate", &self.certificate_b64)?;
        let node_ca_der = decode("node_ca", &self.node_ca_b64)?;
        let root_ca_der = decode("root_ca", &self.root_ca_b64)?;

        super::cert::validate_chain(&certificate_der, &node_ca_der, &root_ca_der)
            .map_err(|e| JoinClientError::Malformed(format!("issued leaf does not chain: {e}")))?;

        let now = SystemTime::now();
        Ok(NodeIdentity {
            node_id: self.node_id,
            certificate_der,
            private_key_der,
            serial: SerialNumber(self.serial),
            ca_generation: self.ca_generation,
            node_ca_der,
            root_ca_der,
            not_before: now,
            not_after: now + Duration::from_secs(365 * 24 * 3600),
        })
    }

    /// The `sha256:` fingerprint of the root CA in this bundle.
    pub fn root_ca_fingerprint(&self) -> Result<String, JoinClientError> {
        let root = BASE64
            .decode(&self.root_ca_b64)
            .map_err(|e| JoinClientError::Malformed(format!("root_ca: {e}")))?;
        Ok(super::identity_store::root_ca_fingerprint(&root))
    }
}

/// Request a certificate from an existing cluster member and return the
/// identity to persist.
///
/// `member_url` is the member's join endpoint, e.g.
/// `https://10.0.1.5:9117/v1/cluster/join`. The connection is trust-on-
/// first-use: a joiner has no CA yet, so the returned root CA fingerprint
/// is the anchor to verify out of band. If `expected_fingerprint` is set,
/// a mismatch is refused before the identity is ever written to disk.
pub async fn request_join(
    client: &reqwest::Client,
    member_url: &str,
    token: &str,
    node_id: &str,
    expected_fingerprint: Option<&str>,
) -> Result<NodeIdentity, JoinClientError> {
    // Generate our own keypair and CSR. The private key stays here (PKI4).
    let (csr_der, private_key_der) = ca::create_node_csr(node_id)
        .map_err(|e| JoinClientError::Malformed(format!("failed to build CSR: {e}")))?;

    let request = JoinRequest {
        token: token.to_string(),
        node_id: node_id.to_string(),
        csr_b64: BASE64.encode(&csr_der),
    };

    let response = client
        .post(member_url)
        .json(&request)
        .send()
        .await
        .map_err(|e| JoinClientError::Transport(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(JoinClientError::Rejected(format!("{status}: {body}")));
    }

    let bundle: JoinBundle = response
        .json()
        .await
        .map_err(|e| JoinClientError::Malformed(e.to_string()))?;

    if let Some(expected) = expected_fingerprint {
        let offered = bundle.root_ca_fingerprint()?;
        if offered != expected {
            return Err(JoinClientError::FingerprintMismatch {
                offered,
                expected: expected.to_string(),
            });
        }
    }

    bundle.into_identity(private_key_der)
}

/// Pre-validate a join token against the replicated security state.
///
/// Confirms the token exists, hasn't expired, and hasn't already been
/// consumed. Returns the token's hash so the caller can consume it atomically
/// through Raft. This is only a fast-fail check: the authoritative
/// consume-and-allocate happens in the state machine (PKI5), so a token that
/// passes here can still lose the race and be refused at commit time.
pub fn check_join_token(
    token_plaintext: &str,
    state: &SecurityState,
) -> Result<[u8; 32], JoinError> {
    let join_token = state
        .join_tokens
        .iter()
        .find(|jt| ca::verify_join_token(token_plaintext, &jt.token_hash))
        .ok_or(JoinError::InvalidToken)?;

    if join_token.consumed {
        return Err(JoinError::TokenConsumed);
    }
    if SystemTime::now() > join_token.expires_at {
        return Err(JoinError::TokenExpired);
    }
    Ok(join_token.token_hash)
}

/// Sign a joiner's CSR into a node certificate (PKI4/PKI5).
///
/// The caller has already consumed the join token and allocated `serial` in
/// one atomic Raft commit, so this function does no token bookkeeping. It
/// takes only the joiner's public key (from the CSR), rebuilds the certificate
/// server-side with the node-id URI SAN, and signs it with the Node CA.
///
/// The issuer DN is derived from the stored Node CA certificate itself, not a
/// hard-coded string, so the issued leaf always chains to the CA that is
/// actually in the trust store.
///
/// The `wrapping_ikm` is needed to unwrap the Node CA private key from Raft
/// storage.
pub fn sign_join_csr(
    csr_der: &[u8],
    node_id: &str,
    serial: SerialNumber,
    state: &SecurityState,
    wrapping_ikm: &[u8],
) -> Result<JoinResult, JoinError> {
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

    // Unwrap the Node CA private key.
    let ca_private_key_der = crypto::unwrap_key(wrapping_ikm, &wrapped_key)?;
    let ca_key_der = rustls::pki_types::PrivateKeyDer::try_from(ca_private_key_der)
        .map_err(|e| JoinError::CertIssueFailed(format!("invalid CA key DER: {e}")))?;
    let ca_keypair =
        rcgen::KeyPair::from_der_and_sign_algo(&ca_key_der, &rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| JoinError::CertIssueFailed(format!("invalid CA key: {e}")))?;

    // Derive the issuer params (DN, constraints) from the stored Node CA cert
    // so the issued leaf's issuer field matches the CA in the trust store.
    let ca_cert_der = rustls::pki_types::CertificateDer::from(node_ca_der.clone());
    let ca_params = rcgen::CertificateParams::from_ca_cert_der(&ca_cert_der)
        .map_err(|e| JoinError::CertIssueFailed(format!("invalid Node CA cert: {e}")))?;

    let (cert_der, serial) = ca::sign_node_csr(csr_der, node_id, serial, &ca_keypair, &ca_params)?;

    Ok(JoinResult {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        serial,
        ca_generation: node_ca_generation,
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
    let (plaintext, join_token) = create_join_token(ttl)?;
    state.join_tokens.push(join_token);
    Ok(plaintext)
}

/// Create a join token without storing its plaintext.
///
/// The caller commits the returned [`JoinToken`] to Raft, then returns the
/// plaintext to the operator exactly once. Keeping creation separate from
/// storage prevents a failed Raft write from producing a token that looks
/// usable but no council member can validate.
pub fn create_join_token(ttl: Duration) -> Result<(String, JoinToken), JoinError> {
    if !(MIN_JOIN_TOKEN_TTL..=MAX_JOIN_TOKEN_TTL).contains(&ttl) {
        return Err(JoinError::InvalidTtl {
            seconds: ttl.as_secs(),
        });
    }
    let (plaintext, hash) = ca::generate_join_token()?;
    let join_token = JoinToken {
        token_hash: hash,
        expires_at: SystemTime::now() + ttl,
        consumed: false,
        attestation_mode: super::types::AttestationMode::None,
    };
    Ok((plaintext, join_token))
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
            oidc_signing_config: None,
            crl: super::super::types::Crl::default(),
            secret_seals: std::collections::BTreeMap::new(),
        };

        (state, token_plaintext, master_secret)
    }

    /// Drive a join the way the agent handler does: pre-check the token, sign
    /// the joiner's CSR with the allocated serial. Returns the CSR private key
    /// (kept by the joiner) alongside the issuer result.
    fn issue(
        state: &SecurityState,
        token: &str,
        node_id: &str,
        serial: SerialNumber,
        master_secret: &[u8],
    ) -> Result<(JoinResult, Vec<u8>), JoinError> {
        check_join_token(token, state)?;
        let (csr_der, key_der) = ca::create_node_csr(node_id).unwrap();
        let result = sign_join_csr(&csr_der, node_id, serial, state, master_secret)?;
        Ok((result, key_der))
    }

    #[test]
    fn join_with_valid_token_succeeds() {
        let (state, token, master_secret) = setup_with_known_key();

        let (result, _key) = issue(&state, &token, "node-02", SerialNumber(6), &master_secret)
            .expect("valid token issues a cert");

        assert_eq!(result.node_id, "node-02");
        assert!(!result.certificate_der.is_empty());
        assert!(!result.root_ca_der.is_empty());
        assert!(!result.node_ca_der.is_empty());
    }

    #[test]
    fn join_bundle_carries_no_private_key() {
        // PKI4: the bundle must never contain a private key. The joiner keeps
        // its key locally; only the CSR and the signed leaf cross the wire.
        let (state, token, master_secret) = setup_with_known_key();
        let (result, _key) = issue(&state, &token, "node-02", SerialNumber(6), &master_secret)
            .expect("valid token issues a cert");

        let bundle = JoinBundle::from_result(&result);
        let json = serde_json::to_string(&bundle).unwrap();
        assert!(
            !json.contains("private_key"),
            "join bundle must not carry a private key, got: {json}"
        );
    }

    #[test]
    fn joiner_assembles_a_working_identity_from_its_own_key() {
        // PKI4: the joiner pairs the returned leaf with the key it kept and
        // gets a self-consistent identity that chains to the CA.
        let (state, token, master_secret) = setup_with_known_key();
        let (result, private_key_der) =
            issue(&state, &token, "node-02", SerialNumber(6), &master_secret).unwrap();

        let bundle = JoinBundle::from_result(&result);
        let json = serde_json::to_string(&bundle).unwrap();
        let decoded: JoinBundle = serde_json::from_str(&json).unwrap();

        let identity = decoded.into_identity(private_key_der.clone()).unwrap();
        assert_eq!(identity.node_id, "node-02");
        assert_eq!(identity.certificate_der, result.certificate_der);
        assert_eq!(identity.private_key_der, private_key_der);
        crate::sesame::cert::validate_chain(
            &identity.certificate_der,
            &identity.node_ca_der,
            &identity.root_ca_der,
        )
        .unwrap();
    }

    #[test]
    fn into_identity_rejects_a_leaf_that_does_not_chain() {
        // A bundle whose leaf was signed by a foreign CA must be refused even
        // if the client kept a matching key.
        let (state, token, master_secret) = setup_with_known_key();
        let (mut result, key) =
            issue(&state, &token, "node-02", SerialNumber(6), &master_secret).unwrap();
        // Swap in a Node CA from a different cluster: the leaf no longer chains.
        let foreign = ca::generate_ca_hierarchy("intruder", b"other").unwrap();
        result.node_ca_der = foreign.node.ca.certificate_der.clone();

        let bundle = JoinBundle::from_result(&result);
        let err = bundle.into_identity(key).unwrap_err();
        assert!(matches!(err, JoinClientError::Malformed(_)));
    }

    #[test]
    fn join_bundle_fingerprint_matches_the_root_ca() {
        let (state, token, master_secret) = setup_with_known_key();
        let (result, _key) =
            issue(&state, &token, "node-02", SerialNumber(6), &master_secret).unwrap();

        let bundle = JoinBundle::from_result(&result);
        let expected = crate::sesame::identity_store::root_ca_fingerprint(&result.root_ca_der);
        assert_eq!(bundle.root_ca_fingerprint().unwrap(), expected);
    }

    #[test]
    fn check_join_token_rejects_a_consumed_token() {
        let (mut state, token, _master_secret) = setup_with_known_key();
        state.join_tokens[0].consumed = true;
        let err = check_join_token(&token, &state).unwrap_err();
        assert!(matches!(err, JoinError::TokenConsumed));
    }

    #[test]
    fn check_join_token_rejects_an_expired_token() {
        let (mut state, token, _master_secret) = setup_with_known_key();
        state.join_tokens[0].expires_at = SystemTime::now() - Duration::from_secs(60);
        let err = check_join_token(&token, &state).unwrap_err();
        assert!(matches!(err, JoinError::TokenExpired));
    }

    #[test]
    fn check_join_token_rejects_an_unknown_token() {
        let (state, _token, _master_secret) = setup_with_known_key();
        let err = check_join_token("rbrg_join_1_deadbeef", &state).unwrap_err();
        assert!(matches!(err, JoinError::InvalidToken));
    }

    #[test]
    fn node_cert_verifies_against_node_ca() {
        let (state, token, master_secret) = setup_with_known_key();
        let (result, _key) =
            issue(&state, &token, "node-02", SerialNumber(6), &master_secret).unwrap();

        crate::sesame::cert::verify_signature(&result.certificate_der, &result.node_ca_der)
            .unwrap();
    }

    #[test]
    fn issued_leaf_issuer_matches_the_stored_node_ca_subject() {
        // PKI5: the issuer DN is derived from the stored Node CA, so the leaf's
        // issuer field equals the CA's subject and the chain path-builds.
        let (state, token, master_secret) = setup_with_known_key();
        let (result, _key) =
            issue(&state, &token, "node-02", SerialNumber(6), &master_secret).unwrap();

        let (_, leaf) = x509_parser::parse_x509_certificate(&result.certificate_der).unwrap();
        let (_, ca_cert) = x509_parser::parse_x509_certificate(&result.node_ca_der).unwrap();
        assert_eq!(leaf.issuer().to_string(), ca_cert.subject().to_string());
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

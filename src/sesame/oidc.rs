//! OIDC JWT minting and verification for workload identity.
//!
//! Generates Ed25519 signing keypairs, mints JWTs with workload claims,
//! verifies JWT signatures, and produces JWKS endpoint responses. Uses
//! ring directly for Ed25519 — no external JWT crate needed.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair};

use super::crypto;
use super::types::{OidcSigningConfig, WorkloadJwtClaims};

/// Errors from OIDC operations.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("failed to generate OIDC keypair: {0}")]
    KeyGenFailed(String),
    #[error("failed to sign JWT: {0}")]
    SignFailed(String),
    #[error("failed to verify JWT: {0}")]
    VerifyFailed(String),
    #[error("JWT has expired")]
    Expired,
    #[error("invalid JWT format")]
    InvalidFormat,
    #[error("unexpected JWT algorithm: got {got:?}, want {want:?}")]
    UnexpectedAlgorithm { got: String, want: String },
    #[error("unexpected JWT key id: got {got:?}, want {want:?}")]
    UnexpectedKeyId { got: String, want: String },
    #[error("wrong JWT issuer: got {got:?}, want {want:?}")]
    WrongIssuer { got: String, want: String },
    #[error("JWT audience does not contain the expected value {want:?}")]
    WrongAudience { want: String },
    #[error("JWT issued-at is out of bounds")]
    BadIssuedAt,
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),
}

/// Constraints a JWT must satisfy, on top of a valid signature and unexpired
/// `exp`. Hardens verification against tokens minted by the right key but for
/// the wrong purpose: an attacker who obtains a token for one audience can't
/// replay it against another, and a stale or future `iat` is rejected (PKI10).
#[derive(Debug, Clone)]
pub struct JwtConstraints {
    /// The issuer the token's `iss` must equal.
    pub expected_issuer: String,
    /// A value the token's `aud` list must contain.
    pub expected_audience: String,
    /// The JOSE `alg` the header must declare. We only sign EdDSA.
    pub expected_algorithm: String,
    /// If set, the JOSE `kid` the header must declare.
    pub expected_key_id: Option<String>,
    /// Largest tolerated clock skew for a future `iat`, in seconds.
    pub max_clock_skew_secs: u64,
    /// Oldest tolerated `iat` (token age ceiling), in seconds.
    pub max_age_secs: u64,
}

impl JwtConstraints {
    /// Build the standard constraints for a token minted by `config`: the
    /// config's issuer, `spiffe://<cluster>` audience, EdDSA, the config's
    /// key id, and generous-but-bounded `iat` windows.
    pub fn for_config(config: &OidcSigningConfig, cluster_name: &str) -> Self {
        Self {
            expected_issuer: config.issuer.clone(),
            expected_audience: format!("spiffe://{cluster_name}"),
            expected_algorithm: "EdDSA".to_string(),
            expected_key_id: Some(config.key_id.clone()),
            // Five minutes of clock skew, one hour of token age (the mint TTL).
            max_clock_skew_secs: 300,
            max_age_secs: 3600,
        }
    }
}

/// Generate an Ed25519 OIDC signing keypair.
///
/// The private key is wrapped with the cluster's master secret using
/// the same HKDF + AES-256-GCM mechanism as CA private keys. The
/// public key and key ID are stored alongside for JWKS publishing.
pub fn generate_oidc_keypair(
    issuer: &str,
    wrapping_ikm: &[u8],
) -> Result<OidcSigningConfig, OidcError> {
    let rng = SystemRandom::new();
    let pkcs8_doc = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| OidcError::KeyGenFailed("Ed25519 PKCS#8 generation failed".to_string()))?;
    let pkcs8_bytes = pkcs8_doc.as_ref();

    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8_bytes)
        .map_err(|e| OidcError::KeyGenFailed(format!("failed to parse generated PKCS#8: {e}")))?;

    let public_key_bytes = key_pair.public_key().as_ref().to_vec();

    // Key ID: first 16 hex chars of SHA-256 of public key
    let hash = ring::digest::digest(&ring::digest::SHA256, &public_key_bytes);
    let key_id = hex::encode(&hash.as_ref()[..8]);

    let wrapped = crypto::wrap_key(
        wrapping_ikm,
        pkcs8_bytes,
        "reliaburger-oidc-signing-wrap-v1",
    )?;

    Ok(OidcSigningConfig {
        signing_key_wrapped: wrapped,
        public_key_der: public_key_bytes,
        key_id,
        issuer: issuer.to_string(),
    })
}

/// Mint a JWT for a workload identity.
///
/// Signs the claims with the cluster's Ed25519 OIDC signing key.
/// Returns the compact JWT string (header.payload.signature).
pub fn mint_jwt(
    claims: &WorkloadJwtClaims,
    config: &OidcSigningConfig,
    wrapping_ikm: &[u8],
) -> Result<String, OidcError> {
    // Unwrap the Ed25519 private key
    let pkcs8_bytes = crypto::unwrap_key(wrapping_ikm, &config.signing_key_wrapped)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8_bytes)
        .map_err(|e| OidcError::SignFailed(format!("invalid Ed25519 key: {e}")))?;

    // Build header
    let header = serde_json::json!({
        "alg": "EdDSA",
        "typ": "JWT",
        "kid": config.key_id,
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string().as_bytes());

    // Encode claims
    let claims_json =
        serde_json::to_string(claims).map_err(|e| OidcError::SignFailed(e.to_string()))?;
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims_json.as_bytes());

    // Sign: header.claims
    let signing_input = format!("{header_b64}.{claims_b64}");
    let sig = key_pair.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());

    Ok(format!("{signing_input}.{sig_b64}"))
}

/// Verify a JWT signature and decode claims, checking only the signature and
/// `exp`. Prefer [`verify_jwt_with_constraints`] for anything security-
/// sensitive: this omits issuer/audience/algorithm/kid/iat checks.
pub fn verify_jwt(token: &str, config: &OidcSigningConfig) -> Result<WorkloadJwtClaims, OidcError> {
    let claims = verify_signature_and_decode(token, config)?;

    let now = unix_now();
    if claims.exp < now {
        return Err(OidcError::Expired);
    }

    Ok(claims)
}

/// Verify a JWT with full constraints (PKI10).
///
/// The order matters. We parse and check the JOSE header's `alg` and `kid`
/// *before* trusting the signature: refusing an unexpected algorithm up front
/// stops an algorithm-confusion attack where a token declares a weaker or
/// attacker-chosen scheme. Then the Ed25519 signature is verified, the claims
/// decoded, and `iss` / `aud` / `exp` / `iat` are checked against `constraints`.
pub fn verify_jwt_with_constraints(
    token: &str,
    config: &OidcSigningConfig,
    constraints: &JwtConstraints,
) -> Result<WorkloadJwtClaims, OidcError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(OidcError::InvalidFormat);
    }

    // Parse the JOSE header and check alg/kid before trusting the signature.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|_| OidcError::InvalidFormat)?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|_| OidcError::InvalidFormat)?;

    let alg = header.get("alg").and_then(|v| v.as_str()).unwrap_or("");
    if alg != constraints.expected_algorithm {
        return Err(OidcError::UnexpectedAlgorithm {
            got: alg.to_string(),
            want: constraints.expected_algorithm.clone(),
        });
    }
    if let Some(expected_kid) = &constraints.expected_key_id {
        let kid = header.get("kid").and_then(|v| v.as_str()).unwrap_or("");
        if kid != expected_kid {
            return Err(OidcError::UnexpectedKeyId {
                got: kid.to_string(),
                want: expected_kid.clone(),
            });
        }
    }

    let claims = verify_signature_and_decode(token, config)?;

    // Issuer and audience: a token minted for a different issuer or audience
    // is not one we accept even if the signature checks out.
    if claims.iss != constraints.expected_issuer {
        return Err(OidcError::WrongIssuer {
            got: claims.iss.clone(),
            want: constraints.expected_issuer.clone(),
        });
    }
    if !claims
        .aud
        .iter()
        .any(|a| a == &constraints.expected_audience)
    {
        return Err(OidcError::WrongAudience {
            want: constraints.expected_audience.clone(),
        });
    }

    // Time bounds. `exp` in the past is expired; `iat` in the future (beyond
    // skew) or too far in the past (a replayed stale token) is rejected.
    let now = unix_now();
    if claims.exp < now {
        return Err(OidcError::Expired);
    }
    if claims.iat > now.saturating_add(constraints.max_clock_skew_secs) {
        return Err(OidcError::BadIssuedAt);
    }
    if claims.iat < now.saturating_sub(constraints.max_age_secs) {
        return Err(OidcError::BadIssuedAt);
    }

    Ok(claims)
}

/// Verify the Ed25519 signature over `header.payload` and decode the claims.
/// Shared by both verify paths.
fn verify_signature_and_decode(
    token: &str,
    config: &OidcSigningConfig,
) -> Result<WorkloadJwtClaims, OidcError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(OidcError::InvalidFormat);
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| OidcError::InvalidFormat)?;

    let public_key = signature::UnparsedPublicKey::new(&signature::ED25519, &config.public_key_der);
    public_key
        .verify(signing_input.as_bytes(), &sig_bytes)
        .map_err(|_| {
            OidcError::VerifyFailed("Ed25519 signature verification failed".to_string())
        })?;

    let claims_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| OidcError::InvalidFormat)?;
    let claims: WorkloadJwtClaims =
        serde_json::from_slice(&claims_bytes).map_err(|_| OidcError::InvalidFormat)?;

    Ok(claims)
}

/// Seconds since the Unix epoch, saturating to zero before it.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Build a JWKS response for the OIDC discovery endpoint.
///
/// Returns a JSON object containing the Ed25519 public key in
/// RFC 8037 OKP format, suitable for serving at `/.well-known/jwks.json`.
pub fn jwks_response(config: &OidcSigningConfig) -> serde_json::Value {
    let x = URL_SAFE_NO_PAD.encode(&config.public_key_der);
    serde_json::json!({
        "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": x,
            "kid": config.key_id,
            "use": "sig",
            "alg": "EdDSA",
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wrapping_ikm() -> Vec<u8> {
        b"test-oidc-wrapping-material-32b!".to_vec()
    }

    fn test_claims() -> WorkloadJwtClaims {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        WorkloadJwtClaims {
            iss: "https://test.reliaburger.dev".to_string(),
            sub: "spiffe://test/ns/default/app/api".to_string(),
            aud: vec!["spiffe://test".to_string()],
            exp: now + 3600,
            iat: now,
            namespace: "default".to_string(),
            app: "api".to_string(),
            cluster: "test".to_string(),
            node: "node-01".to_string(),
            instance: "api-g1-0".to_string(),
        }
    }

    #[test]
    fn generate_oidc_keypair_produces_valid_config() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();

        assert!(!config.public_key_der.is_empty());
        assert_eq!(config.public_key_der.len(), 32); // Ed25519 public key is 32 bytes
        assert_eq!(config.key_id.len(), 16); // first 8 bytes of SHA-256 = 16 hex chars
        assert_eq!(config.issuer, "https://test.reliaburger.dev");

        // The wrapped key should be unwrappable
        let pkcs8 = crypto::unwrap_key(&ikm, &config.signing_key_wrapped).unwrap();
        assert!(!pkcs8.is_empty());
        // Should parse as a valid Ed25519 keypair
        Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
    }

    #[test]
    fn mint_verify_jwt_round_trip() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();
        let claims = test_claims();

        let token = mint_jwt(&claims, &config, &ikm).unwrap();

        // Token has three dot-separated parts
        assert_eq!(token.split('.').count(), 3);

        // Verify round-trips back to the same claims
        let decoded = verify_jwt(&token, &config).unwrap();
        assert_eq!(decoded, claims);
    }

    #[test]
    fn verify_jwt_wrong_key_fails() {
        let ikm = test_wrapping_ikm();
        let config1 = generate_oidc_keypair("https://one.reliaburger.dev", &ikm).unwrap();
        let config2 = generate_oidc_keypair("https://two.reliaburger.dev", &ikm).unwrap();
        let claims = test_claims();

        let token = mint_jwt(&claims, &config1, &ikm).unwrap();
        let result = verify_jwt(&token, &config2);
        assert!(result.is_err());
    }

    #[test]
    fn verify_jwt_expired() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();
        let mut claims = test_claims();
        claims.exp = 1; // expired long ago

        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        let result = verify_jwt(&token, &config);
        assert!(matches!(result, Err(OidcError::Expired)));
    }

    #[test]
    fn verify_jwt_invalid_format() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();

        assert!(matches!(
            verify_jwt("not-a-jwt", &config),
            Err(OidcError::InvalidFormat)
        ));
        assert!(matches!(
            verify_jwt("a.b", &config),
            Err(OidcError::InvalidFormat)
        ));
        assert!(matches!(
            verify_jwt("a.b.c.d", &config),
            Err(OidcError::InvalidFormat)
        ));
    }

    #[test]
    fn jwks_response_structure() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();

        let jwks = jwks_response(&config);
        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);

        let key = &keys[0];
        assert_eq!(key["kty"], "OKP");
        assert_eq!(key["crv"], "Ed25519");
        assert_eq!(key["alg"], "EdDSA");
        assert_eq!(key["use"], "sig");
        assert_eq!(key["kid"], config.key_id);

        // The "x" field should be base64url of the 32-byte public key
        let x = key["x"].as_str().unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(x).unwrap();
        assert_eq!(decoded, config.public_key_der);
    }

    #[test]
    fn key_id_is_deterministic_from_public_key() {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();

        // Recompute the key_id from the public key
        let hash = ring::digest::digest(&ring::digest::SHA256, &config.public_key_der);
        let expected_kid = hex::encode(&hash.as_ref()[..8]);
        assert_eq!(config.key_id, expected_kid);
    }

    #[test]
    fn base64url_round_trip() {
        let data = b"hello, OIDC world!";
        let encoded = URL_SAFE_NO_PAD.encode(data);
        let decoded = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        assert_eq!(decoded, data);

        // No padding characters
        assert!(!encoded.contains('='));
    }

    // --- PKI10: constrained verification ---

    /// A config whose issuer + cluster name line up with `test_claims`.
    fn constrained_setup() -> (OidcSigningConfig, Vec<u8>, JwtConstraints) {
        let ikm = test_wrapping_ikm();
        let config = generate_oidc_keypair("https://test.reliaburger.dev", &ikm).unwrap();
        let constraints = JwtConstraints::for_config(&config, "test");
        (config, ikm, constraints)
    }

    #[test]
    fn constrained_verify_accepts_a_fully_correct_token() {
        let (config, ikm, constraints) = constrained_setup();
        let token = mint_jwt(&test_claims(), &config, &ikm).unwrap();
        let decoded = verify_jwt_with_constraints(&token, &config, &constraints).unwrap();
        assert_eq!(decoded, test_claims());
    }

    #[test]
    fn constrained_verify_rejects_a_wrong_issuer() {
        let (config, ikm, constraints) = constrained_setup();
        let mut claims = test_claims();
        claims.iss = "https://evil.example".to_string();
        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::WrongIssuer { .. })
        ));
    }

    #[test]
    fn constrained_verify_rejects_a_missing_audience() {
        let (config, ikm, constraints) = constrained_setup();
        let mut claims = test_claims();
        claims.aud = vec!["spiffe://someone-else".to_string()];
        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::WrongAudience { .. })
        ));
    }

    #[test]
    fn constrained_verify_rejects_an_unexpected_algorithm() {
        let (config, ikm, mut constraints) = constrained_setup();
        // The token is minted with EdDSA; demand RS256 and it's refused before
        // the signature is even trusted.
        constraints.expected_algorithm = "RS256".to_string();
        let token = mint_jwt(&test_claims(), &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::UnexpectedAlgorithm { .. })
        ));
    }

    #[test]
    fn constrained_verify_rejects_an_unexpected_key_id() {
        let (config, ikm, mut constraints) = constrained_setup();
        constraints.expected_key_id = Some("not-the-real-kid".to_string());
        let token = mint_jwt(&test_claims(), &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::UnexpectedKeyId { .. })
        ));
    }

    #[test]
    fn constrained_verify_rejects_a_future_issued_at() {
        let (config, ikm, constraints) = constrained_setup();
        let mut claims = test_claims();
        // An hour in the future, well past the 5-minute skew tolerance.
        claims.iat = unix_now() + 3600;
        claims.exp = claims.iat + 3600;
        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::BadIssuedAt)
        ));
    }

    #[test]
    fn constrained_verify_rejects_a_stale_issued_at() {
        let (config, ikm, constraints) = constrained_setup();
        let mut claims = test_claims();
        // Issued two hours ago (older than max_age_secs) but not yet expired.
        let now = unix_now();
        claims.iat = now - 7200;
        claims.exp = now + 3600;
        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::BadIssuedAt)
        ));
    }

    #[test]
    fn constrained_verify_rejects_an_expired_token() {
        let (config, ikm, constraints) = constrained_setup();
        let mut claims = test_claims();
        let now = unix_now();
        claims.iat = now - 100;
        claims.exp = now - 10; // expired ten seconds ago
        let token = mint_jwt(&claims, &config, &ikm).unwrap();
        assert!(matches!(
            verify_jwt_with_constraints(&token, &config, &constraints),
            Err(OidcError::Expired)
        ));
    }
}

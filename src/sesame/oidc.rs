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
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),
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

/// Verify a JWT signature and decode claims.
///
/// Checks the Ed25519 signature against the OIDC public key and
/// validates that the token has not expired.
pub fn verify_jwt(token: &str, config: &OidcSigningConfig) -> Result<WorkloadJwtClaims, OidcError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(OidcError::InvalidFormat);
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| OidcError::InvalidFormat)?;

    // Verify signature
    let public_key = signature::UnparsedPublicKey::new(&signature::ED25519, &config.public_key_der);
    public_key
        .verify(signing_input.as_bytes(), &sig_bytes)
        .map_err(|_| {
            OidcError::VerifyFailed("Ed25519 signature verification failed".to_string())
        })?;

    // Decode claims
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| OidcError::InvalidFormat)?;
    let claims: WorkloadJwtClaims =
        serde_json::from_slice(&claims_bytes).map_err(|_| OidcError::InvalidFormat)?;

    // Check expiry
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if claims.exp < now {
        return Err(OidcError::Expired);
    }

    Ok(claims)
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
}

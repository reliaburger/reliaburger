//! Image signature creation and verification.
//!
//! Supports two signing methods:
//! - **Keyless**: uses the workload's ECDSA P-256 keypair (from SPIFFE
//!   identity) to sign. Verification checks the certificate chain back
//!   to the cluster's root CA.
//! - **External key**: cosign-compatible. Signs with a provided ECDSA
//!   P-256 private key. Verification checks the public key against the
//!   cluster's trust policy.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, KeyPair};

use super::types::{Digest, ImageSignature, SigningMethod, VerificationMaterial};
use crate::config::node::TrustPolicySection;

/// Errors from signing operations.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("failed to parse signing key: {0}")]
    KeyParseFailed(String),
    #[error("signing failed: {0}")]
    SignFailed(String),
    #[error("signature verification failed: {0}")]
    VerifyFailed(String),
    #[error("certificate chain verification failed: {0}")]
    ChainVerifyFailed(String),
    #[error("public key not in trust policy")]
    UntrustedKey,
    #[error("invalid signature format")]
    InvalidFormat,
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// Sign a manifest digest with an ECDSA P-256 private key.
///
/// Returns the DER-encoded signature. The message signed is the
/// UTF-8 bytes of the digest string (e.g. `sha256:abc...`).
pub fn sign_manifest_digest(
    digest: &Digest,
    private_key_pkcs8: &[u8],
) -> Result<Vec<u8>, SigningError> {
    let key_pair = EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        private_key_pkcs8,
        &SystemRandom::new(),
    )
    .map_err(|e| SigningError::KeyParseFailed(e.to_string()))?;

    let sig = key_pair
        .sign(&SystemRandom::new(), digest.as_str().as_bytes())
        .map_err(|_| SigningError::SignFailed("ECDSA signing failed".to_string()))?;

    Ok(sig.as_ref().to_vec())
}

/// Create a keyless signature using a workload's SPIFFE identity.
///
/// Signs the manifest digest with the workload's ECDSA P-256 private
/// key and packages the certificate chain as verification material.
pub fn create_keyless_signature(
    digest: &Digest,
    workload_cert_der: &[u8],
    workload_key_pkcs8: &[u8],
    ca_chain_der: &[Vec<u8>],
    issuer: &str,
    identity: &str,
) -> Result<ImageSignature, SigningError> {
    let sig_bytes = sign_manifest_digest(digest, workload_key_pkcs8)?;

    // Build cert chain: leaf cert + intermediate CAs
    let mut chain = vec![workload_cert_der.to_vec()];
    chain.extend_from_slice(ca_chain_der);

    Ok(ImageSignature {
        method: SigningMethod::Keyless {
            issuer: issuer.to_string(),
            identity: identity.to_string(),
        },
        signature: BASE64.encode(&sig_bytes),
        verification_material: VerificationMaterial::CertificateChain(chain),
        signed_at: std::time::SystemTime::now(),
    })
}

/// Create an external key signature (cosign-compatible).
///
/// Signs with the provided ECDSA P-256 private key and stores the
/// public key as verification material.
pub fn create_external_key_signature(
    digest: &Digest,
    private_key_pkcs8: &[u8],
    key_id: &str,
) -> Result<ImageSignature, SigningError> {
    let key_pair = EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        private_key_pkcs8,
        &SystemRandom::new(),
    )
    .map_err(|e| SigningError::KeyParseFailed(e.to_string()))?;

    let sig_bytes = sign_manifest_digest(digest, private_key_pkcs8)?;
    let public_key = key_pair.public_key().as_ref().to_vec();

    Ok(ImageSignature {
        method: SigningMethod::ExternalKey {
            key_id: key_id.to_string(),
        },
        signature: BASE64.encode(&sig_bytes),
        verification_material: VerificationMaterial::PublicKey(public_key),
        signed_at: std::time::SystemTime::now(),
    })
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verify an image signature against the trust policy.
///
/// Dispatches to keyless or external key verification based on the
/// signing method.
pub fn verify_signature(
    sig: &ImageSignature,
    digest: &Digest,
    trust_policy: &TrustPolicySection,
    root_ca_cert_der: Option<&[u8]>,
) -> Result<(), SigningError> {
    match &sig.method {
        SigningMethod::Keyless { .. } => {
            let root = root_ca_cert_der.ok_or_else(|| {
                SigningError::ChainVerifyFailed("no root CA provided".to_string())
            })?;
            verify_keyless(sig, digest, root)
        }
        SigningMethod::ExternalKey { .. } => verify_external_key(sig, digest, &trust_policy.keys),
    }
}

/// Verify a keyless signature by checking the certificate chain
/// and verifying the ECDSA signature against the leaf cert's public key.
fn verify_keyless(
    sig: &ImageSignature,
    digest: &Digest,
    root_ca_cert_der: &[u8],
) -> Result<(), SigningError> {
    let chain = match &sig.verification_material {
        VerificationMaterial::CertificateChain(chain) => chain,
        _ => return Err(SigningError::InvalidFormat),
    };

    if chain.is_empty() {
        return Err(SigningError::ChainVerifyFailed(
            "empty certificate chain".to_string(),
        ));
    }

    let leaf_cert_der = &chain[0];

    // If we have intermediate CAs in the chain, verify leaf is signed by the first intermediate
    if chain.len() >= 2 {
        crate::sesame::cert::verify_signature(leaf_cert_der, &chain[1]).map_err(|e| {
            SigningError::ChainVerifyFailed(format!("leaf cert not signed by intermediate: {e}"))
        })?;
    }

    // Verify the last cert in chain is signed by root CA
    let last_cert = chain.last().unwrap();
    if last_cert != root_ca_cert_der {
        crate::sesame::cert::verify_signature(last_cert, root_ca_cert_der).map_err(|e| {
            SigningError::ChainVerifyFailed(format!("chain does not chain to root CA: {e}"))
        })?;
    }

    // Extract public key from leaf certificate
    let (_, cert) = x509_parser::parse_x509_certificate(leaf_cert_der)
        .map_err(|e| SigningError::VerifyFailed(format!("failed to parse leaf cert: {e}")))?;
    let public_key_bytes = cert.public_key().subject_public_key.data.as_ref();

    // Verify the signature
    verify_ecdsa_signature(public_key_bytes, digest, &sig.signature)
}

/// Verify an external key signature against the trust policy.
fn verify_external_key(
    sig: &ImageSignature,
    digest: &Digest,
    trusted_keys: &[String],
) -> Result<(), SigningError> {
    let public_key = match &sig.verification_material {
        VerificationMaterial::PublicKey(key) => key,
        _ => return Err(SigningError::InvalidFormat),
    };

    // Check that the public key is in the trust policy
    let key_b64 = BASE64.encode(public_key);
    if !trusted_keys.iter().any(|k| k == &key_b64) {
        return Err(SigningError::UntrustedKey);
    }

    verify_ecdsa_signature(public_key, digest, &sig.signature)
}

/// Verify an ECDSA P-256 SHA-256 signature.
fn verify_ecdsa_signature(
    public_key_bytes: &[u8],
    digest: &Digest,
    signature_b64: &str,
) -> Result<(), SigningError> {
    let sig_bytes = BASE64
        .decode(signature_b64)
        .map_err(|_| SigningError::InvalidFormat)?;

    let public_key =
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, public_key_bytes);
    public_key
        .verify(digest.as_str().as_bytes(), &sig_bytes)
        .map_err(|_| SigningError::VerifyFailed("ECDSA signature verification failed".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::Digest;
    use crate::sesame::ca;

    fn test_digest() -> Digest {
        Digest::from_sha256_hex("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
    }

    /// Generate an ECDSA P-256 keypair for testing, returning PKCS#8 DER.
    fn generate_test_keypair() -> Vec<u8> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        pkcs8.as_ref().to_vec()
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let pkcs8 = generate_test_keypair();
        let digest = test_digest();

        let sig_bytes = sign_manifest_digest(&digest, &pkcs8).unwrap();
        assert!(!sig_bytes.is_empty());

        // Verify with the public key
        let key_pair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .unwrap();
        let pub_key = key_pair.public_key().as_ref();
        let sig_b64 = BASE64.encode(&sig_bytes);
        verify_ecdsa_signature(pub_key, &digest, &sig_b64).unwrap();
    }

    #[test]
    fn create_keyless_signature_verifies() {
        let wrapping_ikm = b"test-wrapping-material-32bytes!";
        let hierarchy = ca::generate_ca_hierarchy("test", wrapping_ikm).unwrap();

        // Use the workload CA to issue a cert (simulating the CSR flow)
        let (cert_der, key_der, _) = ca::issue_node_cert(
            "test-workload",
            crate::sesame::types::SerialNumber(100),
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();

        let digest = test_digest();
        let sig = create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            &[hierarchy.workload.ca.certificate_der.clone()],
            "https://test.reliaburger.dev",
            "spiffe://test/ns/ci/job/build",
        )
        .unwrap();

        assert!(matches!(sig.method, SigningMethod::Keyless { .. }));

        // Verify against root CA
        verify_keyless(&sig, &digest, &hierarchy.root.ca.certificate_der).unwrap();
    }

    #[test]
    fn create_external_key_signature_verifies() {
        let pkcs8 = generate_test_keypair();
        let digest = test_digest();

        let sig = create_external_key_signature(&digest, &pkcs8, "my-ci-key").unwrap();
        assert!(matches!(sig.method, SigningMethod::ExternalKey { .. }));

        // Get the public key for the trust policy
        let key_pair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .unwrap();
        let pub_b64 = BASE64.encode(key_pair.public_key().as_ref());
        let trusted_keys = vec![pub_b64];

        verify_external_key(&sig, &digest, &trusted_keys).unwrap();
    }

    #[test]
    fn verify_keyless_wrong_digest_fails() {
        let wrapping_ikm = b"test-wrapping-material-32bytes!";
        let hierarchy = ca::generate_ca_hierarchy("test", wrapping_ikm).unwrap();

        let (cert_der, key_der, _) = ca::issue_node_cert(
            "test-workload",
            crate::sesame::types::SerialNumber(100),
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();

        let digest = test_digest();
        let sig = create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            &[hierarchy.workload.ca.certificate_der.clone()],
            "https://test.reliaburger.dev",
            "spiffe://test/ns/ci/job/build",
        )
        .unwrap();

        // Verify with a different digest — should fail
        let wrong_digest = Digest::from_sha256_hex(
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let result = verify_keyless(&sig, &wrong_digest, &hierarchy.root.ca.certificate_der);
        assert!(result.is_err());
    }

    #[test]
    fn verify_keyless_wrong_ca_fails() {
        let wrapping_ikm = b"test-wrapping-material-32bytes!";
        let hierarchy = ca::generate_ca_hierarchy("test", wrapping_ikm).unwrap();

        let (cert_der, key_der, _) = ca::issue_node_cert(
            "test-workload",
            crate::sesame::types::SerialNumber(100),
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();

        let digest = test_digest();
        let sig = create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            &[hierarchy.workload.ca.certificate_der.clone()],
            "https://test.reliaburger.dev",
            "spiffe://test/ns/ci/job/build",
        )
        .unwrap();

        // Verify with a different root CA — should fail
        let other_hierarchy = ca::generate_ca_hierarchy("other", wrapping_ikm).unwrap();
        let result = verify_keyless(&sig, &digest, &other_hierarchy.root.ca.certificate_der);
        assert!(result.is_err());
    }

    #[test]
    fn verify_external_key_untrusted_fails() {
        let pkcs8 = generate_test_keypair();
        let digest = test_digest();

        let sig = create_external_key_signature(&digest, &pkcs8, "my-key").unwrap();

        // Empty trust policy — key is not trusted
        let result = verify_external_key(&sig, &digest, &[]);
        assert!(matches!(result, Err(SigningError::UntrustedKey)));
    }

    #[test]
    fn verify_external_key_wrong_digest_fails() {
        let pkcs8 = generate_test_keypair();
        let digest = test_digest();

        let sig = create_external_key_signature(&digest, &pkcs8, "my-key").unwrap();

        let key_pair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .unwrap();
        let pub_b64 = BASE64.encode(key_pair.public_key().as_ref());

        let wrong_digest = Digest::from_sha256_hex(
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let result = verify_external_key(&sig, &wrong_digest, &[pub_b64]);
        assert!(result.is_err());
    }

    #[test]
    fn verify_dispatches_to_keyless() {
        let wrapping_ikm = b"test-wrapping-material-32bytes!";
        let hierarchy = ca::generate_ca_hierarchy("test", wrapping_ikm).unwrap();

        let (cert_der, key_der, _) = ca::issue_node_cert(
            "test-workload",
            crate::sesame::types::SerialNumber(100),
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();

        let digest = test_digest();
        let sig = create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            &[hierarchy.workload.ca.certificate_der.clone()],
            "https://test.reliaburger.dev",
            "spiffe://test/ns/ci/job/build",
        )
        .unwrap();

        let policy = TrustPolicySection::default();
        verify_signature(
            &sig,
            &digest,
            &policy,
            Some(&hierarchy.root.ca.certificate_der),
        )
        .unwrap();
    }

    #[test]
    fn verify_dispatches_to_external_key() {
        let pkcs8 = generate_test_keypair();
        let digest = test_digest();

        let sig = create_external_key_signature(&digest, &pkcs8, "my-key").unwrap();

        let key_pair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .unwrap();
        let pub_b64 = BASE64.encode(key_pair.public_key().as_ref());

        let policy = TrustPolicySection {
            require_signatures: true,
            keys: vec![pub_b64],
        };
        verify_signature(&sig, &digest, &policy, None).unwrap();
    }

    #[test]
    fn sign_with_workload_identity_keypair() {
        // The keypair from create_workload_csr should work for signing
        let uri = crate::sesame::types::SpiffeUri {
            trust_domain: "test".to_string(),
            namespace: "ci".to_string(),
            workload_type: crate::sesame::types::WorkloadType::Job,
            name: "build".to_string(),
        };
        let (_, private_key_der) = crate::sesame::identity::create_workload_csr(&uri).unwrap();

        let digest = test_digest();
        let sig_bytes = sign_manifest_digest(&digest, &private_key_der).unwrap();
        assert!(!sig_bytes.is_empty());
    }
}

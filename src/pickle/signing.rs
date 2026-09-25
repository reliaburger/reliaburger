//! Image signature creation and verification.
//!
//! Supports two signing methods:
//! - **Keyless**: uses the workload's ECDSA P-256 keypair (from SPIFFE
//!   identity) to sign. Verification checks the certificate chain back
//!   to the cluster's root CA.
//! - **External key**: an operator-held ECDSA P-256 key ([`SigningKey`],
//!   used by `relish sign`). The signature is made on the operator's
//!   machine; verification checks the public key against the node's
//!   `[images.trust_policy] keys`. Same curve as cosign keys, but not
//!   cosign's payload format: the signed message is the digest string.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};

use super::types::{Digest, ImageSignature, SigningMethod, VerificationMaterial};
use crate::config::node::TrustPolicySection;

/// Errors from signing operations.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("failed to generate signing key")]
    KeyGenerationFailed,
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
    #[error("signing certificate revoked: {0}")]
    Revoked(String),
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

/// PEM label of a PKCS#8 private key document (what `relish sign keygen`
/// writes and `openssl genpkey` produces).
const PRIVATE_KEY_PEM_TAG: &str = "PRIVATE KEY";

/// An operator's ECDSA P-256 image signing key.
///
/// The private half never leaves the machine that runs `relish sign`: the
/// CLI signs the manifest digest locally and sends only the signature and
/// the public key to the cluster. The cluster trusts the key because its
/// public half is listed in `[images.trust_policy] keys`, which lives in
/// each node's config file rather than behind the API, so a stolen API
/// token alone can't make an image trusted.
pub struct SigningKey {
    pkcs8: Vec<u8>,
    key_pair: EcdsaKeyPair,
}

// Hand-written so the private key never lands in logs or panic messages.
impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("public_key", &self.public_key_base64())
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    /// Generate a fresh random key.
    pub fn generate() -> Result<Self, SigningError> {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &SystemRandom::new(),
        )
        .map_err(|_| SigningError::KeyGenerationFailed)?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// Load a key from a PKCS#8 DER document.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, SigningError> {
        let key_pair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|e| SigningError::KeyParseFailed(format!("not an ECDSA P-256 PKCS#8 key: {e}")))?;
        Ok(Self {
            pkcs8: pkcs8.to_vec(),
            key_pair,
        })
    }

    /// Load a key from the text of a `-----BEGIN PRIVATE KEY-----` PEM file.
    pub fn from_pem(text: &str) -> Result<Self, SigningError> {
        let block = pem::parse(text).map_err(|e| SigningError::KeyParseFailed(e.to_string()))?;
        if block.tag() != PRIVATE_KEY_PEM_TAG {
            return Err(SigningError::KeyParseFailed(format!(
                "expected a {PRIVATE_KEY_PEM_TAG:?} (PKCS#8) PEM block, found {:?}; convert it with `openssl pkcs8 -topk8 -nocrypt`",
                block.tag()
            )));
        }
        Self::from_pkcs8(block.contents())
    }

    /// Encode the key as a PKCS#8 PEM document.
    pub fn to_pem(&self) -> String {
        pem::encode(&pem::Pem::new(PRIVATE_KEY_PEM_TAG, self.pkcs8.clone()))
    }

    /// The public key exactly as `[images.trust_policy] keys` lists it: the
    /// base64 of the 65-byte uncompressed P-256 point.
    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.key_pair.public_key().as_ref())
    }

    /// Sign a manifest digest, producing what `relish sign` submits.
    pub fn sign(&self, digest: &Digest) -> Result<SignatureSubmission, SigningError> {
        let sig = self
            .key_pair
            .sign(&SystemRandom::new(), digest.as_str().as_bytes())
            .map_err(|_| SigningError::SignFailed("ECDSA signing failed".to_string()))?;
        Ok(SignatureSubmission {
            digest: digest.as_str().to_string(),
            public_key: self.public_key_base64(),
            signature: BASE64.encode(sig.as_ref()),
        })
    }
}

/// A detached signature over one manifest digest, as `relish sign` sends it
/// to `POST /v1/identity/sign`. It carries no private key material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureSubmission {
    /// The signed manifest digest (`sha256:…`), never a tag.
    pub digest: String,
    /// Base64 uncompressed P-256 public key (the `trust_policy.keys` form).
    pub public_key: String,
    /// Base64 DER ECDSA signature over the digest string.
    pub signature: String,
}

impl SignatureSubmission {
    /// Check the submission is well formed and that its signature verifies
    /// under the key it carries, returning the digest and the signature to
    /// attach.
    ///
    /// This proves only that the holder of *that* key signed *that* digest.
    /// Whether the key is trusted is the deploy-time trust policy's call, so
    /// an operator can sign images before the key reaches every node's policy.
    pub fn into_verified(self) -> Result<(Digest, ImageSignature), SigningError> {
        let digest = Digest::new(&self.digest)
            .map_err(|e| SigningError::VerifyFailed(format!("invalid digest: {e}")))?;
        let public_key = BASE64
            .decode(&self.public_key)
            .map_err(|_| SigningError::InvalidFormat)?;
        let signature = ImageSignature {
            method: SigningMethod::ExternalKey {
                key_id: key_fingerprint(&public_key),
            },
            signature: self.signature,
            verification_material: VerificationMaterial::PublicKey(public_key),
            signed_at: std::time::SystemTime::now(),
        };
        verify_external_key(&signature, &digest, &[self.public_key])?;
        Ok((digest, signature))
    }
}

/// A short, stable name for a public key: `sha256:` plus the first 16 hex
/// characters of its SHA-256. Shown to operators; never used for trust.
pub fn key_fingerprint(public_key: &[u8]) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, public_key);
    let hex: String = hash.as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verify an image signature against the trust policy.
///
/// Dispatches to keyless or external key verification based on the
/// signing method. `crl` is the cluster revocation list: a keyless
/// signature whose chain contains a revoked certificate fails closed.
pub fn verify_signature(
    sig: &ImageSignature,
    digest: &Digest,
    trust_policy: &TrustPolicySection,
    root_ca_cert_der: Option<&[u8]>,
    crl: Option<&crate::sesame::types::Crl>,
) -> Result<(), SigningError> {
    match &sig.method {
        SigningMethod::Keyless { .. } => {
            let root = root_ca_cert_der.ok_or_else(|| {
                SigningError::ChainVerifyFailed("no root CA provided".to_string())
            })?;
            verify_keyless(sig, digest, root, crl)
        }
        SigningMethod::ExternalKey { .. } => verify_external_key(sig, digest, &trust_policy.keys),
    }
}

/// Verify a keyless signature against the cluster root CA at "now".
///
/// See [`verify_keyless_at`] for the full list of checks. This wrapper
/// pins the clock to the current time; tests inject a clock to assert
/// exact validity windows.
fn verify_keyless(
    sig: &ImageSignature,
    digest: &Digest,
    root_ca_cert_der: &[u8],
    crl: Option<&crate::sesame::types::Crl>,
) -> Result<(), SigningError> {
    verify_keyless_at(
        sig,
        digest,
        root_ca_cert_der,
        crl,
        std::time::SystemTime::now(),
    )
}

/// Verify a keyless signature by validating the full certificate chain and
/// checking the ECDSA signature against the leaf cert's public key.
///
/// A partial chain check is no check: an attacker who controls one link can
/// splice an unrelated intermediate and pass a leaf→first / last→root test
/// while the middle never chains. So this validates *every* adjacent link,
/// every certificate's validity window at `at`, and, on the leaf:
/// - the code-signing extended key usage (an mTLS-only cert is not a signer);
/// - a SPIFFE URI SAN matching the identity the signature declares.
///
/// The revocation (CRL) check is kept from Stage 5 (L17).
fn verify_keyless_at(
    sig: &ImageSignature,
    digest: &Digest,
    root_ca_cert_der: &[u8],
    crl: Option<&crate::sesame::types::Crl>,
    at: std::time::SystemTime,
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

    // Full chain: every adjacent signature + issuer binding, chaining to the
    // trust anchor, and every cert valid at `at`.
    crate::sesame::cert::validate_chain_at(chain, root_ca_cert_der, at)
        .map_err(|e| SigningError::ChainVerifyFailed(e.to_string()))?;

    // The leaf must be authorised to sign artefacts.
    crate::sesame::cert::check_code_signing_eku(leaf_cert_der)
        .map_err(|e| SigningError::ChainVerifyFailed(e.to_string()))?;

    // Identity binding: the leaf's SPIFFE URI SAN must match the identity the
    // signature claims. Without this a valid CA-issued cert for workload A
    // could vouch for a signature attributed to workload B.
    if let SigningMethod::Keyless { identity, .. } = &sig.method {
        let sans = crate::sesame::cert::subject_uri_sans(leaf_cert_der)
            .map_err(|e| SigningError::ChainVerifyFailed(e.to_string()))?;
        if !sans.iter().any(|s| s == identity) {
            return Err(SigningError::ChainVerifyFailed(format!(
                "leaf certificate does not carry the claimed signer identity {identity}"
            )));
        }
    }

    // Revocation: a signature is only as trustworthy as the certificate that
    // made it. If any cert in the chain has been revoked, fail closed (L17).
    if let Some(crl) = crl {
        for cert_der in chain.iter() {
            let serial = crate::sesame::cert::serial_from_der(cert_der)
                .map_err(|e| SigningError::VerifyFailed(format!("failed to read serial: {e}")))?;
            crate::sesame::cert::check_crl(serial, crl)
                .map_err(|e| SigningError::Revoked(e.to_string()))?;
        }
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

    /// The SPIFFE identity a test build signer certifies as.
    fn build_signer_uri() -> crate::sesame::types::SpiffeUri {
        crate::sesame::types::SpiffeUri {
            trust_domain: "test".to_string(),
            namespace: "ci".to_string(),
            workload_type: crate::sesame::types::WorkloadType::Job,
            name: "build-signer".to_string(),
        }
    }

    /// Issue a code-signing leaf under the hierarchy's workload CA for the
    /// given identity, returning `(cert_der, key_der)`. Mirrors what the
    /// council does for the persistent build signer.
    fn issue_codesigning_leaf(
        hierarchy: &ca::CaHierarchy,
        uri: &crate::sesame::types::SpiffeUri,
        serial: u64,
    ) -> (Vec<u8>, Vec<u8>) {
        let (csr_der, key_der) = crate::sesame::identity::create_workload_csr(uri).unwrap();
        let cert_der = crate::sesame::identity::validate_and_sign_csr(
            &csr_der,
            uri,
            crate::sesame::types::SerialNumber(serial),
            crate::sesame::identity::CertUsage::CodeSigning,
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
            std::time::SystemTime::now(),
        )
        .unwrap();
        (cert_der, key_der)
    }

    /// Build a well-formed code-signing keyless signature over `test_digest()`,
    /// returning `(sig, root_ca_der)`.
    fn codesigning_keyless_sig() -> (ImageSignature, Vec<u8>) {
        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-wrapping-material-32bytes!").unwrap();
        let uri = build_signer_uri();
        let (cert_der, key_der) = issue_codesigning_leaf(&hierarchy, &uri, 100);
        let digest = test_digest();
        let sig = create_keyless_signature(
            &digest,
            &cert_der,
            &key_der,
            std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
            "reliaburger-council",
            &uri.to_uri(),
        )
        .unwrap();
        (sig, hierarchy.root.ca.certificate_der.clone())
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
        let (sig, root) = codesigning_keyless_sig();
        assert!(matches!(sig.method, SigningMethod::Keyless { .. }));
        // Full IMG3 validation passes for a well-formed code-signing chain.
        verify_keyless(&sig, &test_digest(), &root, None).unwrap();
    }

    /// Sign `test_digest()` with a fresh operator key, returning the key and
    /// the verified signature the cluster would attach.
    fn operator_signature() -> (SigningKey, ImageSignature) {
        let key = SigningKey::generate().unwrap();
        let (_, sig) = key.sign(&test_digest()).unwrap().into_verified().unwrap();
        (key, sig)
    }

    #[test]
    fn operator_signature_verifies_under_a_policy_listing_its_key() {
        let (key, sig) = operator_signature();
        assert!(matches!(sig.method, SigningMethod::ExternalKey { .. }));
        verify_external_key(&sig, &test_digest(), &[key.public_key_base64()]).unwrap();
    }

    #[test]
    fn signing_key_survives_a_pem_round_trip() {
        let key = SigningKey::generate().unwrap();
        let reloaded = SigningKey::from_pem(&key.to_pem()).unwrap();
        assert_eq!(key.public_key_base64(), reloaded.public_key_base64());
    }

    /// A throwaway key made with
    /// `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256`, and
    /// the public key the documented pipeline
    /// `openssl pkey -in key.pem -pubout -outform DER | tail -c 65 | base64`
    /// printed for it. Test-only: it signs nothing real.
    const OPENSSL_TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg88KY5p0jjyPBySn4
8bfB423Ph6y8VnlWO1ttiJ4Fw6uhRANCAAQgFERVfIBuq9Swzu658XaD0yvCJyyg
gW44LD4On4yfIPRJkluhNQ5G35R5vZyQY5DspOlhl16ImqPQIVADgGQv
-----END PRIVATE KEY-----
";
    const OPENSSL_TEST_PUBLIC_KEY: &str =
        "BCAURFV8gG6r1LDO7rnxdoPTK8InLKCBbjgsPg6fjJ8g9EmSW6E1DkbflHm9nJBjkOyk6WGXXoiao9AhUAOAZC8=";

    #[test]
    fn an_openssl_generated_key_loads_and_matches_the_documented_public_key() {
        let key = SigningKey::from_pem(OPENSSL_TEST_KEY).unwrap();
        assert_eq!(key.public_key_base64(), OPENSSL_TEST_PUBLIC_KEY);
    }

    #[test]
    fn a_sec1_ec_private_key_is_refused_with_a_conversion_hint() {
        let sec1 = OPENSSL_TEST_KEY.replace("PRIVATE KEY", "EC PRIVATE KEY");
        let error = SigningKey::from_pem(&sec1).unwrap_err().to_string();
        assert!(error.contains("openssl pkcs8 -topk8"), "got: {error}");
    }

    #[test]
    fn signing_key_debug_output_omits_the_private_key() {
        let key = SigningKey::generate().unwrap();
        let debug = format!("{key:?}");
        assert!(debug.contains(&key.public_key_base64()));
        assert!(!debug.contains("pkcs8"), "got: {debug}");
    }

    #[test]
    fn a_submission_with_a_tampered_signature_is_refused() {
        let key = SigningKey::generate().unwrap();
        let mut submission = key.sign(&test_digest()).unwrap();
        submission.digest = format!("sha256:{}", "0".repeat(64));
        assert!(submission.into_verified().is_err());
    }

    #[test]
    fn a_submission_naming_a_tag_instead_of_a_digest_is_refused() {
        let key = SigningKey::generate().unwrap();
        let mut submission = key.sign(&test_digest()).unwrap();
        submission.digest = "myapp:v1".to_string();
        assert!(submission.into_verified().is_err());
    }

    #[test]
    fn a_submission_claiming_another_public_key_is_refused() {
        let key = SigningKey::generate().unwrap();
        let other = SigningKey::generate().unwrap();
        let mut submission = key.sign(&test_digest()).unwrap();
        submission.public_key = other.public_key_base64();
        assert!(submission.into_verified().is_err());
    }

    #[test]
    fn verify_keyless_wrong_digest_fails() {
        let (sig, root) = codesigning_keyless_sig();
        let wrong_digest = Digest::from_sha256_hex(
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(verify_keyless(&sig, &wrong_digest, &root, None).is_err());
    }

    #[test]
    fn verify_keyless_wrong_ca_fails() {
        let (sig, _root) = codesigning_keyless_sig();
        // Verify against an unrelated root CA — should fail.
        let other = ca::generate_ca_hierarchy("other", b"test-wrapping-material-32bytes!").unwrap();
        assert!(
            verify_keyless(&sig, &test_digest(), &other.root.ca.certificate_der, None).is_err()
        );
    }

    #[test]
    fn verify_keyless_rejects_a_leaf_without_code_signing_eku() {
        // A leaf issued as an mTLS workload cert (ServerAuth/ClientAuth) is not
        // a code signer, so its signature is rejected (IMG3).
        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-wrapping-material-32bytes!").unwrap();
        let uri = build_signer_uri();
        let (csr_der, key_der) = crate::sesame::identity::create_workload_csr(&uri).unwrap();
        let cert_der = crate::sesame::identity::validate_and_sign_csr(
            &csr_der,
            &uri,
            crate::sesame::types::SerialNumber(100),
            crate::sesame::identity::CertUsage::Mtls,
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
            std::time::SystemTime::now(),
        )
        .unwrap();
        let sig = create_keyless_signature(
            &test_digest(),
            &cert_der,
            &key_der,
            std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
            "reliaburger-council",
            &uri.to_uri(),
        )
        .unwrap();
        let result = verify_keyless(
            &sig,
            &test_digest(),
            &hierarchy.root.ca.certificate_der,
            None,
        );
        assert!(
            result.is_err(),
            "mTLS leaf should not sign images: {result:?}"
        );
    }

    #[test]
    fn verify_keyless_rejects_a_mismatched_signer_identity() {
        // The signature claims one identity but the leaf certifies another.
        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-wrapping-material-32bytes!").unwrap();
        let uri = build_signer_uri();
        let (cert_der, key_der) = issue_codesigning_leaf(&hierarchy, &uri, 100);
        let sig = create_keyless_signature(
            &test_digest(),
            &cert_der,
            &key_der,
            std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
            "reliaburger-council",
            "spiffe://test/ns/ci/job/someone-else",
        )
        .unwrap();
        let result = verify_keyless(
            &sig,
            &test_digest(),
            &hierarchy.root.ca.certificate_der,
            None,
        );
        assert!(
            result.is_err(),
            "identity mismatch must be rejected: {result:?}"
        );
    }

    #[test]
    fn verify_keyless_rejects_a_broken_intermediate_link() {
        // Splice an unrelated workload CA between leaf and root: the leaf→CA
        // signature no longer holds, so the every-link check fails (IMG3).
        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-wrapping-material-32bytes!").unwrap();
        let other = ca::generate_ca_hierarchy("other", b"test-wrapping-material-32bytes!").unwrap();
        let uri = build_signer_uri();
        let (cert_der, key_der) = issue_codesigning_leaf(&hierarchy, &uri, 100);
        // Present the WRONG workload CA in the chain.
        let sig = create_keyless_signature(
            &test_digest(),
            &cert_der,
            &key_der,
            std::slice::from_ref(&other.workload.ca.certificate_der),
            "reliaburger-council",
            &uri.to_uri(),
        )
        .unwrap();
        let result = verify_keyless(
            &sig,
            &test_digest(),
            &hierarchy.root.ca.certificate_der,
            None,
        );
        assert!(result.is_err(), "broken chain must be rejected: {result:?}");
    }

    #[test]
    fn verify_keyless_rejects_an_expired_chain() {
        let (sig, root) = codesigning_keyless_sig();
        // Ten years hence, the (one-hour) leaf has long expired.
        let future =
            std::time::SystemTime::now() + std::time::Duration::from_secs(10 * 365 * 24 * 3600);
        assert!(verify_keyless_at(&sig, &test_digest(), &root, None, future).is_err());
    }

    /// Build a keyless signature over `test_digest()` whose leaf has the given
    /// serial, returning `(sig, root_ca_der, workload_ca_serial)`.
    fn keyless_sig_with_leaf_serial(
        leaf_serial: u64,
    ) -> (ImageSignature, Vec<u8>, crate::sesame::types::SerialNumber) {
        let hierarchy =
            ca::generate_ca_hierarchy("test", b"test-wrapping-material-32bytes!").unwrap();
        let uri = build_signer_uri();
        let (cert_der, key_der) = issue_codesigning_leaf(&hierarchy, &uri, leaf_serial);
        let sig = create_keyless_signature(
            &test_digest(),
            &cert_der,
            &key_der,
            std::slice::from_ref(&hierarchy.workload.ca.certificate_der),
            "reliaburger-council",
            &uri.to_uri(),
        )
        .unwrap();
        let workload_ca_serial =
            crate::sesame::cert::serial_from_der(&hierarchy.workload.ca.certificate_der).unwrap();
        (
            sig,
            hierarchy.root.ca.certificate_der.clone(),
            workload_ca_serial,
        )
    }

    fn crl_revoking(serial: crate::sesame::types::SerialNumber) -> crate::sesame::types::Crl {
        crate::sesame::types::Crl {
            retired_nodes: Default::default(),
            entries: vec![crate::sesame::types::CrlEntry {
                serial,
                issuer: crate::sesame::types::CaRole::Workload,
                revoked_at: std::time::SystemTime::now(),
                reason: "test".to_string(),
                expires_at: None,
            }],
            version: 1,
            updated_at: std::time::SystemTime::now(),
        }
    }

    #[test]
    fn revoked_leaf_certificate_fails_keyless_verification() {
        let (sig, root, _) = keyless_sig_with_leaf_serial(100);
        let crl = crl_revoking(crate::sesame::types::SerialNumber(100));
        let result = verify_keyless(&sig, &test_digest(), &root, Some(&crl));
        assert!(
            matches!(result, Err(SigningError::Revoked(_))),
            "got: {result:?}"
        );
    }

    #[test]
    fn revoked_intermediate_certificate_fails_keyless_verification() {
        let (sig, root, workload_ca_serial) = keyless_sig_with_leaf_serial(100);
        // Revoke the intermediate (workload CA), not the leaf.
        let crl = crl_revoking(workload_ca_serial);
        let result = verify_keyless(&sig, &test_digest(), &root, Some(&crl));
        assert!(
            matches!(result, Err(SigningError::Revoked(_))),
            "got: {result:?}"
        );
    }

    #[test]
    fn an_empty_crl_passes_keyless_verification() {
        let (sig, root, _) = keyless_sig_with_leaf_serial(100);
        let empty = crate::sesame::types::Crl::default();
        verify_keyless(&sig, &test_digest(), &root, Some(&empty)).unwrap();
    }

    #[test]
    fn verify_external_key_untrusted_fails() {
        let (_key, sig) = operator_signature();
        // Empty trust policy: the key is not trusted.
        let result = verify_external_key(&sig, &test_digest(), &[]);
        assert!(matches!(result, Err(SigningError::UntrustedKey)));
    }

    #[test]
    fn verify_external_key_wrong_digest_fails() {
        let (key, sig) = operator_signature();
        let wrong_digest = Digest::from_sha256_hex(
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let result = verify_external_key(&sig, &wrong_digest, &[key.public_key_base64()]);
        assert!(result.is_err());
    }

    #[test]
    fn verify_dispatches_to_keyless() {
        let (sig, root) = codesigning_keyless_sig();
        let policy = TrustPolicySection::default();
        verify_signature(&sig, &test_digest(), &policy, Some(&root), None).unwrap();
    }

    #[test]
    fn verify_dispatches_to_external_key() {
        let (key, sig) = operator_signature();
        let policy = TrustPolicySection {
            require_signatures: true,
            keys: vec![key.public_key_base64()],
        };
        verify_signature(&sig, &test_digest(), &policy, None, None).unwrap();
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

//! Dual Ed25519 signature creation and verification for release binaries.
//!
//! Every network-distributed binary carries two detached signatures over its
//! raw bytes: one from the release key set compiled into the running binary
//! (proves it came from a Reliaburger release), and one from the external key
//! configured in node.toml (proves this operator approved it). Air-gapped
//! upgrades (`relish upgrade start --binary`) skip the external signature.
//!
//! Signatures travel in a JSON envelope stored next to each versioned binary
//! (`bun-v0.2.0.sig`) and embedded in upgrade directives.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::UpgradeError;

/// Prefix for encoded Ed25519 public keys in config files and key files.
pub const KEY_PREFIX: &str = "ed25519:";

/// A raw Ed25519 public key.
pub type PublicKey = [u8; 32];

/// Detached signature envelope for one binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// Envelope format version. Currently 1.
    pub schema: u32,
    /// Hex-encoded SHA-256 of the binary bytes.
    pub sha256: String,
    /// Base64 Ed25519 signature from a release key.
    pub embedded: String,
    /// Base64 Ed25519 signature from the operator's external key.
    /// `None` for air-gapped releases.
    pub external: Option<String>,
}

impl SignatureEnvelope {
    /// Read an envelope from a `.sig` file.
    pub fn load(path: &Path) -> Result<Self, UpgradeError> {
        let contents = std::fs::read_to_string(path)?;
        serde_json::from_str(&contents).map_err(|e| UpgradeError::InvalidEnvelope {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Write the envelope to a `.sig` file.
    pub fn store(&self, path: &Path) -> Result<(), UpgradeError> {
        // Serialising a struct with only string/int fields cannot fail.
        let json = serde_json::to_string_pretty(self).expect("envelope serialises");
        std::fs::write(path, json)?;
        Ok(())
    }
}

/// Hex-encoded SHA-256 of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Parse an `ed25519:BASE64` public key string into raw bytes.
pub fn parse_public_key(input: &str) -> Result<PublicKey, UpgradeError> {
    let invalid = |reason: &str| UpgradeError::InvalidKey {
        input: input.to_string(),
        reason: reason.to_string(),
    };
    let encoded = input
        .strip_prefix(KEY_PREFIX)
        .ok_or_else(|| invalid("missing ed25519: prefix"))?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| invalid("invalid base64"))?;
    bytes
        .try_into()
        .map_err(|_| invalid("key must be exactly 32 bytes"))
}

/// Encode a raw public key as `ed25519:BASE64`.
pub fn encode_public_key(key: &PublicKey) -> String {
    format!("{KEY_PREFIX}{}", BASE64.encode(key))
}

/// Generate a fresh Ed25519 keypair.
///
/// Returns the PKCS#8 private key document (store it somewhere safe, never
/// in the repository) and the raw public key.
pub fn generate_keypair() -> Result<(Vec<u8>, PublicKey), UpgradeError> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| UpgradeError::KeyGeneration)?;
    let key_pair =
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| UpgradeError::KeyGeneration)?;
    let public: PublicKey = key_pair
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| UpgradeError::KeyGeneration)?;
    Ok((pkcs8.as_ref().to_vec(), public))
}

/// Sign bytes with a PKCS#8 Ed25519 private key. Returns the base64 signature.
pub fn sign(pkcs8: &[u8], bytes: &[u8]) -> Result<String, UpgradeError> {
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| UpgradeError::InvalidKey {
        input: "<pkcs8 private key>".to_string(),
        reason: "not a valid Ed25519 PKCS#8 document".to_string(),
    })?;
    Ok(BASE64.encode(key_pair.sign(bytes)))
}

fn verify_one(key: &PublicKey, bytes: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, key)
        .verify(bytes, signature)
        .is_ok()
}

/// Verify a binary against its signature envelope.
///
/// Checks, in order:
/// 1. The SHA-256 of `bytes` matches the envelope (cheap, fails fast, and
///    means signature checks below run against known-identified content).
/// 2. The embedded signature verifies against at least one release key.
/// 3. For network upgrades (`network = true`): an external key must be
///    configured and the external signature must verify against it.
///    Air-gapped upgrades skip this — but if both an external key and an
///    external signature are present anyway, a mismatch is still an error
///    rather than being silently ignored.
pub fn verify_binary(
    bytes: &[u8],
    envelope: &SignatureEnvelope,
    release_keys: &[PublicKey],
    external_key: Option<&PublicKey>,
    network: bool,
) -> Result<(), UpgradeError> {
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(&envelope.sha256) {
        return Err(UpgradeError::HashMismatch {
            expected: envelope.sha256.clone(),
            actual,
        });
    }

    let embedded_sig = BASE64
        .decode(&envelope.embedded)
        .map_err(|_| UpgradeError::EmbeddedSignatureInvalid)?;
    if !release_keys
        .iter()
        .any(|key| verify_one(key, bytes, &embedded_sig))
    {
        return Err(UpgradeError::EmbeddedSignatureInvalid);
    }

    match (network, external_key, envelope.external.as_deref()) {
        (true, None, _) => Err(UpgradeError::ExternalKeyRequired),
        (true, Some(_), None) => Err(UpgradeError::ExternalSignatureInvalid),
        (_, Some(key), Some(sig)) => {
            let sig = BASE64
                .decode(sig)
                .map_err(|_| UpgradeError::ExternalSignatureInvalid)?;
            if verify_one(key, bytes, &sig) {
                Ok(())
            } else {
                Err(UpgradeError::ExternalSignatureInvalid)
            }
        }
        // Air-gapped without both parts present: embedded signature suffices.
        (false, _, _) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct TestKeys {
        release_pkcs8: Vec<u8>,
        release_public: PublicKey,
        external_pkcs8: Vec<u8>,
        external_public: PublicKey,
    }

    fn test_keys() -> TestKeys {
        let (release_pkcs8, release_public) = generate_keypair().unwrap();
        let (external_pkcs8, external_public) = generate_keypair().unwrap();
        TestKeys {
            release_pkcs8,
            release_public,
            external_pkcs8,
            external_public,
        }
    }

    fn envelope_for(bytes: &[u8], keys: &TestKeys, external: bool) -> SignatureEnvelope {
        SignatureEnvelope {
            schema: 1,
            sha256: sha256_hex(bytes),
            embedded: sign(&keys.release_pkcs8, bytes).unwrap(),
            external: external.then(|| sign(&keys.external_pkcs8, bytes).unwrap()),
        }
    }

    #[test]
    fn verifies_correct_dual_signatures() {
        let keys = test_keys();
        let bytes = b"the binary";
        let envelope = envelope_for(bytes, &keys, true);
        verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            true,
        )
        .unwrap();
    }

    #[test]
    fn rejects_wrong_hash_before_checking_signatures() {
        let keys = test_keys();
        let bytes = b"the binary";
        let mut envelope = envelope_for(bytes, &keys, true);
        envelope.sha256 = sha256_hex(b"something else");
        let err = verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::HashMismatch { .. }));
    }

    #[test]
    fn rejects_tampered_binary_bytes() {
        let keys = test_keys();
        let bytes = b"the binary";
        let mut envelope = envelope_for(bytes, &keys, true);
        // Attacker recomputes the hash for tampered bytes but cannot re-sign.
        let tampered = b"the b1nary";
        envelope.sha256 = sha256_hex(tampered);
        let err = verify_binary(
            tampered,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::EmbeddedSignatureInvalid));
    }

    #[test]
    fn rejects_signature_from_unknown_release_key() {
        let keys = test_keys();
        let impostor = test_keys(); // different keypair set
        let bytes = b"the binary";
        let envelope = envelope_for(bytes, &impostor, true);
        let err = verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&impostor.external_public),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::EmbeddedSignatureInvalid));
    }

    #[test]
    fn accepts_any_key_in_the_release_set() {
        let old = test_keys();
        let new = test_keys();
        let bytes = b"the binary";
        // Signed with the second key of a two-key set (key rotation window).
        let envelope = envelope_for(bytes, &new, false);
        verify_binary(
            bytes,
            &envelope,
            &[old.release_public, new.release_public],
            None,
            false,
        )
        .unwrap();
    }

    #[test]
    fn network_upgrade_requires_external_key_and_signature() {
        let keys = test_keys();
        let bytes = b"the binary";

        // No external key configured on the node.
        let envelope = envelope_for(bytes, &keys, true);
        let err = verify_binary(bytes, &envelope, &[keys.release_public], None, true).unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalKeyRequired));

        // Key configured but the envelope carries no external signature.
        let envelope = envelope_for(bytes, &keys, false);
        let err = verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalSignatureInvalid));
    }

    #[test]
    fn rejects_wrong_external_signature() {
        let keys = test_keys();
        let other = test_keys();
        let bytes = b"the binary";
        let mut envelope = envelope_for(bytes, &keys, true);
        envelope.external = Some(sign(&other.external_pkcs8, bytes).unwrap());
        let err = verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalSignatureInvalid));
    }

    #[test]
    fn airgapped_upgrade_skips_external_signature() {
        let keys = test_keys();
        let bytes = b"the binary";
        let envelope = envelope_for(bytes, &keys, false);
        // No external signature, no external key: fine when network = false.
        verify_binary(bytes, &envelope, &[keys.release_public], None, false).unwrap();
        // External key configured but envelope has no external signature:
        // still fine air-gapped.
        verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            false,
        )
        .unwrap();
    }

    #[test]
    fn airgapped_still_rejects_a_present_but_wrong_external_signature() {
        let keys = test_keys();
        let other = test_keys();
        let bytes = b"the binary";
        let mut envelope = envelope_for(bytes, &keys, true);
        envelope.external = Some(sign(&other.external_pkcs8, bytes).unwrap());
        let err = verify_binary(
            bytes,
            &envelope,
            &[keys.release_public],
            Some(&keys.external_public),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalSignatureInvalid));
    }

    #[test]
    fn parses_ed25519_prefixed_keys_and_rejects_bad_input() {
        let keys = test_keys();
        let encoded = encode_public_key(&keys.release_public);
        assert_eq!(parse_public_key(&encoded).unwrap(), keys.release_public);

        for bad in [
            "",
            "no-prefix-at-all",
            "ed25519:",
            "ed25519:!!!not base64!!!",
            "ed25519:c2hvcnQ=", // valid base64, wrong length
        ] {
            assert!(
                parse_public_key(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn envelope_roundtrips_through_disk() {
        let keys = test_keys();
        let bytes = b"the binary";
        let envelope = envelope_for(bytes, &keys, true);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bun-v0.2.0.sig");
        envelope.store(&path).unwrap();
        assert_eq!(SignatureEnvelope::load(&path).unwrap(), envelope);
    }

    #[test]
    fn corrupt_envelope_reports_invalid_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bun-v0.2.0.sig");
        std::fs::write(&path, "not json").unwrap();
        let err = SignatureEnvelope::load(&path).unwrap_err();
        assert!(matches!(err, UpgradeError::InvalidEnvelope { .. }));
    }
}

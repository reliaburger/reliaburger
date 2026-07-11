//! Secret encryption using age.
//!
//! Handles `ENC[AGE:...]` encrypted environment variables. Each cluster
//! has a cluster-wide age keypair, and namespaces can optionally have
//! their own keypair for isolation.

use std::io::{Read as _, Write as _};

use age::secrecy::ExposeSecret;
use base64::Engine as _;

use super::crypto;
use super::types::{AgeKeyScope, AgeKeypair};

/// Errors from secret operations.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("failed to generate age keypair: {0}")]
    KeyGenFailed(String),
    #[error("failed to encrypt secret: {0}")]
    EncryptFailed(String),
    #[error("failed to decrypt secret: {0}")]
    DecryptFailed(String),
    #[error("invalid ENC[AGE:...] format: {0}")]
    InvalidFormat(String),
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),
}

const ENC_PREFIX: &str = "ENC[AGE:";
const ENC_SUFFIX: &str = "]";

/// Generate a new age keypair for secret encryption.
///
/// The private key is wrapped with the provided IKM. Returns the
/// keypair struct for storage in Raft.
pub fn generate_age_keypair(
    scope: AgeKeyScope,
    wrapping_ikm: &[u8],
    generation: u64,
) -> Result<(AgeKeypair, age::x25519::Identity), SecretError> {
    let identity = age::x25519::Identity::generate();
    let public_key = identity.to_public().to_string();
    let private_key_str = identity.to_string();

    let wrap_info = match &scope {
        AgeKeyScope::ClusterWide => "reliaburger-age-cluster-wrap-v1".to_string(),
        AgeKeyScope::Namespace(ns) => format!("reliaburger-age-ns-{ns}-wrap-v1"),
    };

    let wrapped = crypto::wrap_key(
        wrapping_ikm,
        private_key_str.expose_secret().as_bytes(),
        &wrap_info,
    )?;

    Ok((
        AgeKeypair {
            scope,
            public_key,
            private_key_wrapped: wrapped,
            generation,
            read_only: false,
        },
        identity,
    ))
}

/// Unwrap an age private key from Raft storage.
pub fn unwrap_age_identity(
    keypair: &AgeKeypair,
    wrapping_ikm: &[u8],
) -> Result<age::x25519::Identity, SecretError> {
    let private_key_bytes = crypto::unwrap_key(wrapping_ikm, &keypair.private_key_wrapped)?;
    let private_key_str = String::from_utf8(private_key_bytes)
        .map_err(|e| SecretError::DecryptFailed(format!("invalid UTF-8 in age key: {e}")))?;
    let identity: age::x25519::Identity = private_key_str
        .parse()
        .map_err(|e| SecretError::DecryptFailed(format!("invalid age identity: {e}")))?;
    Ok(identity)
}

/// Encrypt a plaintext secret with an age public key.
///
/// Returns the encrypted value in `ENC[AGE:...]` format, suitable
/// for embedding in app config files.
pub fn encrypt_secret(plaintext: &str, public_key: &str) -> Result<String, SecretError> {
    let recipient: age::x25519::Recipient = public_key
        .parse()
        .map_err(|e| SecretError::EncryptFailed(format!("invalid public key: {e}")))?;

    let encryptor =
        age::Encryptor::with_recipients(vec![Box::new(recipient)]).expect("at least one recipient");

    let mut encrypted = vec![];
    let mut writer = encryptor
        .wrap_output(
            age::armor::ArmoredWriter::wrap_output(&mut encrypted, age::armor::Format::AsciiArmor)
                .map_err(|e| SecretError::EncryptFailed(e.to_string()))?,
        )
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;

    writer
        .write_all(plaintext.as_bytes())
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;
    let armored_writer = writer
        .finish()
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;
    armored_writer
        .finish()
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;

    let armored =
        String::from_utf8(encrypted).map_err(|e| SecretError::EncryptFailed(e.to_string()))?;

    // Encode as base64 for single-line embedding
    let encoded = base64::engine::general_purpose::STANDARD.encode(armored.as_bytes());

    Ok(format!("{ENC_PREFIX}{encoded}{ENC_SUFFIX}"))
}

/// Decrypt an `ENC[AGE:...]` value using an age identity (private key).
pub fn decrypt_secret(
    encrypted_value: &str,
    identity: &age::x25519::Identity,
) -> Result<String, SecretError> {
    let inner = parse_enc_age(encrypted_value)?;

    // Decode base64 → armored age
    let armored_bytes = base64::engine::general_purpose::STANDARD
        .decode(inner)
        .map_err(|e| SecretError::InvalidFormat(format!("invalid base64: {e}")))?;

    let decryptor = match age::Decryptor::new(age::armor::ArmoredReader::new(&armored_bytes[..]))
        .map_err(|e| SecretError::DecryptFailed(e.to_string()))?
    {
        age::Decryptor::Recipients(d) => d,
        _ => {
            return Err(SecretError::DecryptFailed(
                "expected recipients-based encryption".to_string(),
            ));
        }
    };

    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| SecretError::DecryptFailed(e.to_string()))?;

    let mut plaintext = String::new();
    reader
        .read_to_string(&mut plaintext)
        .map_err(|e| SecretError::DecryptFailed(e.to_string()))?;

    Ok(plaintext)
}

/// Extract the inner content from an `ENC[AGE:...]` string.
fn parse_enc_age(s: &str) -> Result<&str, SecretError> {
    let rest = s
        .strip_prefix(ENC_PREFIX)
        .ok_or_else(|| SecretError::InvalidFormat(format!("missing {ENC_PREFIX} prefix")))?;
    let inner = rest
        .strip_suffix(ENC_SUFFIX)
        .ok_or_else(|| SecretError::InvalidFormat(format!("missing {ENC_SUFFIX} suffix")))?;
    Ok(inner)
}

/// Check if a string is an `ENC[AGE:...]` encrypted value.
pub fn is_encrypted(s: &str) -> bool {
    s.starts_with(ENC_PREFIX) && s.ends_with(ENC_SUFFIX)
}

/// Seal data with an age public key (used for sealing the root CA backup).
pub fn seal_with_age(data: &[u8], public_key: &str) -> Result<Vec<u8>, SecretError> {
    let recipient: age::x25519::Recipient = public_key
        .parse()
        .map_err(|e| SecretError::EncryptFailed(format!("invalid public key: {e}")))?;

    let encryptor =
        age::Encryptor::with_recipients(vec![Box::new(recipient)]).expect("at least one recipient");

    let mut output = vec![];
    let mut writer = encryptor
        .wrap_output(&mut output)
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;
    writer
        .write_all(data)
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;
    writer
        .finish()
        .map_err(|e| SecretError::EncryptFailed(e.to_string()))?;

    Ok(output)
}

/// Unseal data encrypted with `seal_with_age`.
pub fn unseal_with_age(
    sealed: &[u8],
    identity: &age::x25519::Identity,
) -> Result<Vec<u8>, SecretError> {
    let decryptor =
        match age::Decryptor::new(sealed).map_err(|e| SecretError::DecryptFailed(e.to_string()))? {
            age::Decryptor::Recipients(d) => d,
            _ => {
                return Err(SecretError::DecryptFailed(
                    "expected recipients-based encryption".to_string(),
                ));
            }
        };

    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| SecretError::DecryptFailed(e.to_string()))?;

    let mut output = vec![];
    reader
        .read_to_end(&mut output)
        .map_err(|e| SecretError::DecryptFailed(e.to_string()))?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keypair() -> (age::x25519::Identity, String) {
        let id = age::x25519::Identity::generate();
        let pk = id.to_public().to_string();
        (id, pk)
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let (identity, public_key) = test_keypair();
        let plaintext = "database-password-123";

        let encrypted = encrypt_secret(plaintext, &public_key).unwrap();
        assert!(encrypted.starts_with(ENC_PREFIX));
        assert!(encrypted.ends_with(ENC_SUFFIX));

        let decrypted = decrypt_secret(&encrypted, &identity).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn enc_age_format_parsing() {
        assert!(is_encrypted("ENC[AGE:abc123]"));
        assert!(!is_encrypted("plain-value"));
        assert!(!is_encrypted("ENC[AGE:no-closing-bracket"));
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let (_identity1, public_key1) = test_keypair();
        let (identity2, _public_key2) = test_keypair();

        let encrypted = encrypt_secret("secret", &public_key1).unwrap();
        let result = decrypt_secret(&encrypted, &identity2);
        assert!(result.is_err());
    }

    #[test]
    fn generate_and_unwrap_age_keypair() {
        let wrapping_ikm = b"test-wrapping-material";
        let (keypair, identity) =
            generate_age_keypair(AgeKeyScope::ClusterWide, wrapping_ikm, 0).unwrap();

        assert!(!keypair.public_key.is_empty());
        assert_eq!(keypair.scope, AgeKeyScope::ClusterWide);
        assert_eq!(keypair.generation, 0);

        // Unwrap and compare
        let unwrapped = unwrap_age_identity(&keypair, wrapping_ikm).unwrap();
        assert_eq!(
            unwrapped.to_public().to_string(),
            identity.to_public().to_string()
        );
    }

    #[test]
    fn namespace_scoped_key_isolation() {
        let wrapping_ikm = b"test-wrapping-material";
        let (_kp_a, id_a) = generate_age_keypair(
            AgeKeyScope::Namespace("team-a".to_string()),
            wrapping_ikm,
            0,
        )
        .unwrap();
        let (_kp_b, id_b) = generate_age_keypair(
            AgeKeyScope::Namespace("team-b".to_string()),
            wrapping_ikm,
            0,
        )
        .unwrap();

        let pk_a = id_a.to_public().to_string();

        // Encrypt with team-a's key
        let encrypted = encrypt_secret("team-a-secret", &pk_a).unwrap();

        // Team A can decrypt
        let decrypted = decrypt_secret(&encrypted, &id_a).unwrap();
        assert_eq!(decrypted, "team-a-secret");

        // Team B cannot
        let result = decrypt_secret(&encrypted, &id_b);
        assert!(result.is_err());
    }

    #[test]
    fn seal_unseal_round_trip() {
        let (identity, public_key) = test_keypair();
        let data = b"root-ca-private-key-bytes";

        let sealed = seal_with_age(data, &public_key).unwrap();
        assert_ne!(sealed, data);

        let unsealed = unseal_with_age(&sealed, &identity).unwrap();
        assert_eq!(unsealed, data);
    }

    /// PKI8: a secret sealed under generation N must still decrypt after a
    /// rotation adds generation N+1 and retires N. The agent decrypts by
    /// trying every live identity newest-first, so both the old and the new
    /// secret open; a node that only holds the new key cannot read the old
    /// secret — which is why finalize must wait until every workload has
    /// re-sealed.
    #[test]
    fn secrets_decrypt_under_any_live_generation() {
        fn try_all(encrypted: &str, identities: &[age::x25519::Identity]) -> Option<String> {
            identities
                .iter()
                .find_map(|id| decrypt_secret(encrypted, id).ok())
        }

        let ikm = b"test-wrapping-material";
        let (kp0, id0) = generate_age_keypair(AgeKeyScope::ClusterWide, ikm, 0).unwrap();
        let (kp1, id1) = generate_age_keypair(AgeKeyScope::ClusterWide, ikm, 1).unwrap();

        // A secret sealed before the rotation, and one sealed after.
        let old_secret = encrypt_secret("db-password", &kp0.public_key).unwrap();
        let new_secret = encrypt_secret("api-key", &kp1.public_key).unwrap();

        // The agent holds both generations, newest first.
        let live = [id1, id0];
        assert_eq!(try_all(&old_secret, &live).as_deref(), Some("db-password"));
        assert_eq!(try_all(&new_secret, &live).as_deref(), Some("api-key"));

        // Holding only the new key can't open the pre-rotation secret — the
        // data loss PKI8 guards against.
        assert!(try_all(&old_secret, &live[..1]).is_none());
    }
}

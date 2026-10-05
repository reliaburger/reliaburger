//! Secret encryption using age.
//!
//! Handles `ENC[AGE:...]` encrypted environment variables. Each cluster
//! has a cluster-wide age keypair, and namespaces can optionally have
//! their own keypair for isolation.

use std::io::{Read as _, Write as _};

use age::secrecy::ExposeSecret;
use base64::Engine as _;

use super::crypto;
use super::types::{AgeKeyScope, AgeKeypair, ResealedSecret, SecurityState};

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

/// Every age identity that may decrypt a value stored in `namespace`,
/// newest generation first.
///
/// A namespace with a key of its own gets only its own identities; one
/// without gets the cluster-wide ones (see
/// [`SecurityState::decryption_keypairs`]). A key that won't unwrap with
/// `wrapping_ikm` is left out, so the caller fails closed on the value it
/// can't open rather than on the whole set.
pub fn namespace_identities(
    state: &SecurityState,
    namespace: &str,
    wrapping_ikm: &[u8],
) -> Vec<age::x25519::Identity> {
    state
        .decryption_keypairs(namespace)
        .into_iter()
        .filter_map(|kp| unwrap_age_identity(kp, wrapping_ikm).ok())
        .collect()
}

/// Seal each encrypted environment value in `apps` again under
/// `public_key`, the namespace's new key (F05 I4).
///
/// `identities` are the keys that opened the values until now: the
/// cluster-wide ones. A value none of them opens is left alone, since it
/// didn't decrypt before the namespace opted in either, and it still fails
/// its deploy closed afterwards. Plaintext lives only inside this call.
pub fn reseal_namespace_values<'a>(
    apps: impl IntoIterator<
        Item = (
            &'a crate::meat::types::AppId,
            &'a crate::config::app::AppSpec,
        ),
    >,
    identities: &[age::x25519::Identity],
    public_key: &str,
) -> Result<Vec<ResealedSecret>, SecretError> {
    let mut resealed = Vec::new();
    for (app_id, spec) in apps {
        for (env_key, value) in &spec.env {
            if !value.is_encrypted() {
                continue;
            }
            let previous = value.as_str();
            let Some(plaintext) = identities
                .iter()
                .find_map(|id| decrypt_secret(previous, id).ok())
            else {
                continue;
            };
            resealed.push(ResealedSecret {
                app_id: app_id.clone(),
                env_key: env_key.clone(),
                previous: previous.to_string(),
                sealed: encrypt_secret(&plaintext, public_key)?,
            });
        }
    }
    Ok(resealed)
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

/// Unseal bytes encrypted with [`seal_with_age`].
/// This cryptographic primitive does not restore a cluster CA or validate its
/// state. A supported operator recovery workflow remains separate work.
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

    fn app_with_env(env: &[(&str, &str)]) -> crate::config::app::AppSpec {
        let mut text = String::from("image = \"web:v1\"\n[env]\n");
        for (key, value) in env {
            text.push_str(&format!("{key} = \"{value}\"\n"));
        }
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn resealing_moves_a_cluster_sealed_value_under_the_namespace_key() {
        let ikm = b"test-wrapping-material";
        let (cluster, cluster_id) = generate_age_keypair(AgeKeyScope::ClusterWide, ikm, 0).unwrap();
        let (team_a, team_a_id) =
            generate_age_keypair(AgeKeyScope::Namespace("team-a".into()), ikm, 0).unwrap();
        let sealed = encrypt_secret("db-password", &cluster.public_key).unwrap();
        let app_id = crate::meat::types::AppId::new("web", "team-a");
        let spec = app_with_env(&[("DB_PASSWORD", &sealed), ("MODE", "plain")]);

        let resealed = reseal_namespace_values(
            [(&app_id, &spec)],
            std::slice::from_ref(&cluster_id),
            &team_a.public_key,
        )
        .unwrap();

        assert_eq!(resealed.len(), 1, "plain values aren't touched");
        assert_eq!(resealed[0].app_id, app_id);
        assert_eq!(resealed[0].env_key, "DB_PASSWORD");
        assert_eq!(resealed[0].previous, sealed);
        assert_eq!(
            decrypt_secret(&resealed[0].sealed, &team_a_id).unwrap(),
            "db-password"
        );
        assert!(
            decrypt_secret(&resealed[0].sealed, &cluster_id).is_err(),
            "the cluster key no longer opens the re-sealed value"
        );
    }

    #[test]
    fn resealing_leaves_a_value_no_cluster_key_opens() {
        let ikm = b"test-wrapping-material";
        let (_, cluster_id) = generate_age_keypair(AgeKeyScope::ClusterWide, ikm, 0).unwrap();
        let (team_a, _) =
            generate_age_keypair(AgeKeyScope::Namespace("team-a".into()), ikm, 0).unwrap();
        let (_, stranger) = test_keypair();
        let foreign = encrypt_secret("not ours", &stranger).unwrap();
        let app_id = crate::meat::types::AppId::new("web", "team-a");
        let spec = app_with_env(&[("TOKEN", &foreign)]);

        let resealed =
            reseal_namespace_values([(&app_id, &spec)], &[cluster_id], &team_a.public_key).unwrap();

        assert!(resealed.is_empty());
    }

    #[test]
    fn namespace_identities_follow_the_namespaces_own_keys() {
        let ikm = b"test-wrapping-material";
        let (cluster, _) = generate_age_keypair(AgeKeyScope::ClusterWide, ikm, 0).unwrap();
        let (team_a, team_a_id) =
            generate_age_keypair(AgeKeyScope::Namespace("team-a".into()), ikm, 0).unwrap();
        let state = SecurityState {
            age_keypairs: vec![cluster.clone(), team_a],
            ..SecurityState::default()
        };
        let cluster_sealed = encrypt_secret("shared", &cluster.public_key).unwrap();

        let team_a_ids = namespace_identities(&state, "team-a", ikm);
        assert_eq!(team_a_ids.len(), 1);
        assert_eq!(
            team_a_ids[0].to_public().to_string(),
            team_a_id.to_public().to_string()
        );
        assert!(decrypt_secret(&cluster_sealed, &team_a_ids[0]).is_err());

        let team_b_ids = namespace_identities(&state, "team-b", ikm);
        assert_eq!(team_b_ids.len(), 1);
        assert_eq!(
            decrypt_secret(&cluster_sealed, &team_b_ids[0]).unwrap(),
            "shared"
        );
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

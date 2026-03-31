//! Raft log encryption at rest.
//!
//! Encrypts Raft log entries with AES-256-GCM before writing to disk
//! and decrypts them on read. The encryption key is derived via HKDF
//! from the node's certificate private key.

use super::crypto::{self, CryptoError};

/// Errors from Raft log encryption.
#[derive(Debug, thiserror::Error)]
pub enum RaftEncryptionError {
    #[error("encryption failed: {0}")]
    EncryptFailed(#[from] CryptoError),
    #[error("decryption failed: entry may be corrupted")]
    DecryptFailed,
    #[error("invalid encrypted entry format")]
    InvalidFormat,
}

/// HKDF info string for Raft log encryption key derivation.
const RAFT_LOG_HKDF_INFO: &str = "reliaburger-raft-log-encryption-v1";

/// An encrypted Raft log entry.
///
/// Stored on disk instead of the plaintext entry. The nonce and salt
/// are needed for decryption.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedEntry {
    /// AES-256-GCM ciphertext (includes 16-byte auth tag).
    pub ciphertext: Vec<u8>,
    /// 96-bit nonce used for this entry.
    pub nonce: [u8; 12],
    /// HKDF salt used to derive the encryption key.
    pub hkdf_salt: [u8; 32],
}

/// Derive the Raft log encryption key from a node's identity.
///
/// Uses HKDF-SHA256 with the node cert private key as IKM. Each node
/// derives its own key, so Raft entries encrypted on one node cannot
/// be decrypted on another (each node re-encrypts when receiving
/// entries via replication).
pub fn derive_log_encryption_key(
    node_private_key_der: &[u8],
    salt: &[u8; 32],
) -> Result<[u8; 32], RaftEncryptionError> {
    Ok(crypto::hkdf_derive_key(
        node_private_key_der,
        salt,
        RAFT_LOG_HKDF_INFO,
    )?)
}

/// Encrypt a serialised Raft log entry.
///
/// The `node_private_key_der` is used as HKDF input key material.
/// A random salt is generated per-entry for key derivation.
pub fn encrypt_entry(
    plaintext: &[u8],
    node_private_key_der: &[u8],
) -> Result<EncryptedEntry, RaftEncryptionError> {
    let hkdf_salt: [u8; 32] = crypto::random_bytes()?;
    let key = derive_log_encryption_key(node_private_key_der, &hkdf_salt)?;
    let (ciphertext, nonce) = crypto::aes_256_gcm_encrypt(&key, plaintext)?;

    Ok(EncryptedEntry {
        ciphertext,
        nonce,
        hkdf_salt,
    })
}

/// Decrypt a Raft log entry.
pub fn decrypt_entry(
    encrypted: &EncryptedEntry,
    node_private_key_der: &[u8],
) -> Result<Vec<u8>, RaftEncryptionError> {
    let key = derive_log_encryption_key(node_private_key_der, &encrypted.hkdf_salt)?;
    crypto::aes_256_gcm_decrypt(&key, &encrypted.ciphertext, &encrypted.nonce)
        .map_err(|_| RaftEncryptionError::DecryptFailed)
}

/// Check that an encrypted entry cannot be read as plaintext.
///
/// Returns true if the ciphertext does not contain the expected
/// plaintext bytes — used in tests to verify encryption is working.
pub fn is_encrypted(ciphertext: &[u8], plaintext: &[u8]) -> bool {
    if ciphertext.len() < plaintext.len() {
        return true;
    }
    // Check that the plaintext doesn't appear as a substring
    !ciphertext
        .windows(plaintext.len())
        .any(|window| window == plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_node_key() -> Vec<u8> {
        // Simulate a node's private key DER
        b"node-01-private-key-der-for-testing-purposes".to_vec()
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let node_key = test_node_key();
        let plaintext = b"raft log entry: app deploy web replicas=3";

        let encrypted = encrypt_entry(plaintext, &node_key).unwrap();
        let decrypted = decrypt_entry(&encrypted, &node_key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypted_entry_is_not_plaintext() {
        let node_key = test_node_key();
        let plaintext = b"secret configuration data in raft log";

        let encrypted = encrypt_entry(plaintext, &node_key).unwrap();

        assert!(is_encrypted(&encrypted.ciphertext, plaintext));
    }

    #[test]
    fn different_nodes_produce_different_ciphertext() {
        let node_key_1 = b"node-01-private-key".to_vec();
        let node_key_2 = b"node-02-private-key".to_vec();
        let plaintext = b"same raft entry on both nodes";

        let encrypted_1 = encrypt_entry(plaintext, &node_key_1).unwrap();
        let encrypted_2 = encrypt_entry(plaintext, &node_key_2).unwrap();

        // Different keys → different ciphertext
        assert_ne!(encrypted_1.ciphertext, encrypted_2.ciphertext);

        // But both decrypt correctly with their own key
        let decrypted_1 = decrypt_entry(&encrypted_1, &node_key_1).unwrap();
        let decrypted_2 = decrypt_entry(&encrypted_2, &node_key_2).unwrap();
        assert_eq!(decrypted_1, plaintext);
        assert_eq!(decrypted_2, plaintext);
    }

    #[test]
    fn wrong_node_key_fails_decryption() {
        let node_key_1 = b"node-01-private-key".to_vec();
        let node_key_2 = b"node-02-private-key".to_vec();
        let plaintext = b"encrypted with node-01's key";

        let encrypted = encrypt_entry(plaintext, &node_key_1).unwrap();
        let result = decrypt_entry(&encrypted, &node_key_2);
        assert!(result.is_err());
    }
}

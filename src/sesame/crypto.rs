//! Cryptographic primitives for Sesame.
//!
//! HKDF key derivation, AES-256-GCM encrypt/decrypt, and key wrapping.
//! Built on `ring` — the same library that rustls uses internally.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf::{self, HKDF_SHA256, Salt};
use ring::rand::{SecureRandom, SystemRandom};

use super::types::WrappedKey;

/// Errors from cryptographic operations.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("HKDF key derivation failed")]
    HkdfFailed,
    #[error("AES-256-GCM encryption failed")]
    EncryptionFailed,
    #[error("AES-256-GCM decryption failed: ciphertext is corrupted or key is wrong")]
    DecryptionFailed,
    #[error("random number generation failed")]
    RngFailed,
    #[error("key wrapping failed: {0}")]
    WrapFailed(String),
    #[error("key unwrapping failed: {0}")]
    UnwrapFailed(String),
}

/// Derive a 256-bit key from input key material using HKDF-SHA256.
///
/// The `salt` adds randomness (should be stored alongside the output).
/// The `info` string binds the derived key to a specific purpose,
/// preventing cross-protocol attacks.
pub fn hkdf_derive_key(ikm: &[u8], salt: &[u8; 32], info: &str) -> Result<[u8; 32], CryptoError> {
    let salt = Salt::new(HKDF_SHA256, salt);
    let prk = salt.extract(ikm);

    let info_bytes = [info.as_bytes()];
    let okm = prk
        .expand(&info_bytes, HkdfLen)
        .map_err(|_| CryptoError::HkdfFailed)?;

    let mut key = [0u8; 32];
    okm.fill(&mut key).map_err(|_| CryptoError::HkdfFailed)?;
    Ok(key)
}

/// Helper type for HKDF output length (32 bytes = AES-256 key).
struct HkdfLen;

impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        32
    }
}

/// Encrypt plaintext with AES-256-GCM.
///
/// Returns `(ciphertext, nonce)`. The nonce is randomly generated
/// and must be stored alongside the ciphertext for decryption.
pub fn aes_256_gcm_encrypt(
    key: &[u8; 32],
    plaintext: &[u8],
) -> Result<(Vec<u8>, [u8; 12]), CryptoError> {
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| CryptoError::RngFailed)?;

    let unbound_key =
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::EncryptionFailed)?;
    let sealing_key = LessSafeKey::new(unbound_key);

    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| CryptoError::EncryptionFailed)?;

    Ok((in_out, nonce_bytes))
}

/// Decrypt ciphertext with AES-256-GCM.
///
/// The `ciphertext` must include the 16-byte authentication tag
/// appended by `aes_256_gcm_encrypt`.
pub fn aes_256_gcm_decrypt(
    key: &[u8; 32],
    ciphertext: &[u8],
    nonce_bytes: &[u8; 12],
) -> Result<Vec<u8>, CryptoError> {
    let unbound_key =
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::DecryptionFailed)?;
    let opening_key = LessSafeKey::new(unbound_key);

    let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
    let mut in_out = ciphertext.to_vec();
    let plaintext = opening_key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    Ok(plaintext.to_vec())
}

/// Wrap (encrypt) a private key using HKDF + AES-256-GCM.
///
/// The `ikm` is the input key material (e.g. a master secret or node cert
/// private key). The `info` string scopes the derived wrapping key to a
/// specific purpose (e.g. `"reliaburger-node-ca-wrap-v1"`).
pub fn wrap_key(ikm: &[u8], plaintext_key: &[u8], info: &str) -> Result<WrappedKey, CryptoError> {
    let rng = SystemRandom::new();
    let mut hkdf_salt = [0u8; 32];
    rng.fill(&mut hkdf_salt)
        .map_err(|_| CryptoError::RngFailed)?;

    let wrapping_key = hkdf_derive_key(ikm, &hkdf_salt, info)?;
    let (ciphertext, nonce) = aes_256_gcm_encrypt(&wrapping_key, plaintext_key)?;

    Ok(WrappedKey {
        ciphertext,
        nonce,
        hkdf_salt,
        hkdf_info: info.to_string(),
    })
}

/// Unwrap (decrypt) a private key that was wrapped with `wrap_key`.
pub fn unwrap_key(ikm: &[u8], wrapped: &WrappedKey) -> Result<Vec<u8>, CryptoError> {
    let wrapping_key = hkdf_derive_key(ikm, &wrapped.hkdf_salt, &wrapped.hkdf_info)?;
    aes_256_gcm_decrypt(&wrapping_key, &wrapped.ciphertext, &wrapped.nonce)
}

/// Generate cryptographically secure random bytes.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let rng = SystemRandom::new();
    let mut buf = [0u8; N];
    rng.fill(&mut buf).map_err(|_| CryptoError::RngFailed)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_deterministic_with_same_inputs() {
        let ikm = b"master-secret";
        let salt = [42u8; 32];
        let info = "test-purpose";

        let key1 = hkdf_derive_key(ikm, &salt, info).unwrap();
        let key2 = hkdf_derive_key(ikm, &salt, info).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn hkdf_different_salt_produces_different_key() {
        let ikm = b"master-secret";
        let salt1 = [1u8; 32];
        let salt2 = [2u8; 32];
        let info = "test-purpose";

        let key1 = hkdf_derive_key(ikm, &salt1, info).unwrap();
        let key2 = hkdf_derive_key(ikm, &salt2, info).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn hkdf_different_info_produces_different_key() {
        let ikm = b"master-secret";
        let salt = [0u8; 32];

        let key1 = hkdf_derive_key(ikm, &salt, "purpose-a").unwrap();
        let key2 = hkdf_derive_key(ikm, &salt, "purpose-b").unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn aes_gcm_encrypt_decrypt_round_trip() {
        let key = [0xABu8; 32];
        let plaintext = b"hello, reliaburger!";

        let (ciphertext, nonce) = aes_256_gcm_encrypt(&key, plaintext).unwrap();
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);

        let decrypted = aes_256_gcm_decrypt(&key, &ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn aes_gcm_wrong_key_fails_decryption() {
        let key = [0xABu8; 32];
        let wrong_key = [0xCDu8; 32];
        let plaintext = b"secret data";

        let (ciphertext, nonce) = aes_256_gcm_encrypt(&key, plaintext).unwrap();
        let result = aes_256_gcm_decrypt(&wrong_key, &ciphertext, &nonce);
        assert!(result.is_err());
    }

    #[test]
    fn aes_gcm_tampered_ciphertext_fails_decryption() {
        let key = [0xABu8; 32];
        let plaintext = b"integrity check";

        let (mut ciphertext, nonce) = aes_256_gcm_encrypt(&key, plaintext).unwrap();
        // Flip a bit in the ciphertext
        ciphertext[0] ^= 0x01;

        let result = aes_256_gcm_decrypt(&key, &ciphertext, &nonce);
        assert!(result.is_err());
    }

    #[test]
    fn wrap_unwrap_round_trip() {
        let ikm = b"node-cert-private-key-der";
        let secret_key = b"this-is-a-ca-private-key-to-wrap";
        let info = "reliaburger-node-ca-wrap-v1";

        let wrapped = wrap_key(ikm, secret_key, info).unwrap();
        assert_eq!(wrapped.hkdf_info, info);
        // Ciphertext is longer than plaintext (due to GCM tag)
        assert!(wrapped.ciphertext.len() > secret_key.len());

        let unwrapped = unwrap_key(ikm, &wrapped).unwrap();
        assert_eq!(unwrapped, secret_key);
    }

    #[test]
    fn wrap_unwrap_with_wrong_ikm_fails() {
        let ikm = b"correct-key-material";
        let wrong_ikm = b"wrong-key-material!!";
        let secret = b"sensitive-data";

        let wrapped = wrap_key(ikm, secret, "test").unwrap();
        let result = unwrap_key(wrong_ikm, &wrapped);
        assert!(result.is_err());
    }

    #[test]
    fn random_bytes_produces_different_values() {
        let a: [u8; 32] = random_bytes().unwrap();
        let b: [u8; 32] = random_bytes().unwrap();
        // Astronomically unlikely to be equal
        assert_ne!(a, b);
    }
}

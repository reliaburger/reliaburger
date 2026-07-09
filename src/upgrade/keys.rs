//! The release key set compiled into the binary.
//!
//! Binaries only upgrade to binaries signed by one of these keys. The set
//! (rather than a single key) allows rotation: ship a binary trusting both
//! the old and new key, sign the next release with the new key only, then
//! drop the old key a release later.

use crate::config::node::UpgradeSection;

use super::error::UpgradeError;
use super::signing::{PublicKey, parse_public_key};

/// Ed25519 public keys trusted to sign release binaries.
///
/// The matching private keys live outside the repository (see the release
/// process notes in docs/README.md). Verified by `embedded_keys_parse`.
pub const EMBEDDED_RELEASE_KEYS: &[&str] =
    &["ed25519:kdNmHSKOupiiF2i5vCyNrNMmEeagWZzB4DOm/w3a1IY="];

/// The release keys this binary trusts.
///
/// In debug builds `upgrades.release_keys_override` (node.toml) replaces the
/// compiled-in set so integration tests can sign with throwaway keys. Release
/// builds ignore the override with a warning: a config file must never be
/// able to widen what a production binary trusts.
pub fn release_keys(section: &UpgradeSection) -> Result<Vec<PublicKey>, UpgradeError> {
    if let Some(override_keys) = &section.release_keys_override {
        if cfg!(debug_assertions) {
            return override_keys.iter().map(|k| parse_public_key(k)).collect();
        }
        eprintln!("bun: warning: upgrades.release_keys_override is ignored in release builds");
    }
    EMBEDDED_RELEASE_KEYS
        .iter()
        .map(|k| parse_public_key(k))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::signing::{encode_public_key, generate_keypair};

    #[test]
    fn embedded_keys_parse() {
        let keys = release_keys(&UpgradeSection::default()).unwrap();
        assert!(!keys.is_empty());
    }

    #[test]
    fn override_replaces_embedded_keys_in_debug_builds() {
        let (_, public) = generate_keypair().unwrap();
        let section = UpgradeSection {
            release_keys_override: Some(vec![encode_public_key(&public)]),
            ..UpgradeSection::default()
        };
        let keys = release_keys(&section).unwrap();
        // cargo test always runs debug builds, so the override applies.
        assert_eq!(keys, vec![public]);
    }

    #[test]
    fn invalid_override_key_is_an_error() {
        let section = UpgradeSection {
            release_keys_override: Some(vec!["not-a-key".to_string()]),
            ..UpgradeSection::default()
        };
        assert!(release_keys(&section).is_err());
    }
}

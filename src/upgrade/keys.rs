//! The release key set compiled into the binary.
//!
//! Binaries only upgrade to binaries signed by one of these keys. The set
//! (rather than a single key) allows rotation: ship a binary trusting both
//! the old and new key, sign the next release with the new key only, then
//! drop the old key a release later.
//!
//! A release train's nightly build is the one exception. It's compiled with
//! the `nightly-signing` feature and trusts the nightly keys *instead of* the
//! release keys, so the release key never signs unreleased code and no
//! released binary ever accepts a nightly one (#1363).

use crate::config::node::UpgradeSection;

use super::error::UpgradeError;
use super::signing::{PublicKey, parse_public_key};

/// Ed25519 public keys trusted to sign release binaries.
///
/// The matching private keys live outside the repository (see the release
/// process notes in docs/README.md). Verified by `embedded_keys_parse`.
pub const EMBEDDED_RELEASE_KEYS: &[&str] =
    &["ed25519:zSUgsFfmv0WohbjRJE7FJf/xgLIgMuK7AbnDgOdduRM="];

/// Ed25519 public keys trusted by a release train's nightly build, and by
/// nothing else.
///
/// Only a build with the `nightly-signing` feature trusts them, and that
/// build trusts no release key. `scripts/release/package.py` reads this list
/// to check the nightly signing key, and refuses a key that's in both lists.
/// The maintainers created the key on 27 October 2026 (docs/releasing.md,
/// "Nightly signing key"); its private half is the `RELIABURGER_NIGHTLY_KEY`
/// secret.
pub const NIGHTLY_SIGNING_KEYS: &[&str] = &["ed25519:MwQBdUzLbKUieHJ054IgcZ5LA/ICyGLLz12G4ifP+bs="];

/// The key set this build was compiled to trust.
fn compiled_keys() -> &'static [&'static str] {
    if cfg!(feature = "nightly-signing") {
        NIGHTLY_SIGNING_KEYS
    } else {
        EMBEDDED_RELEASE_KEYS
    }
}

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
    compiled_keys()
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
        let keys: Vec<PublicKey> = EMBEDDED_RELEASE_KEYS
            .iter()
            .map(|key| parse_public_key(key).unwrap())
            .collect();
        assert!(!keys.is_empty());
    }

    #[test]
    fn nightly_keys_parse() {
        for key in NIGHTLY_SIGNING_KEYS {
            parse_public_key(key).unwrap();
        }
    }

    #[test]
    fn no_key_is_both_a_release_and_a_nightly_key() {
        for key in NIGHTLY_SIGNING_KEYS {
            assert!(
                !EMBEDDED_RELEASE_KEYS.contains(key),
                "{key} is a release key"
            );
        }
    }

    /// The keys a default build (no override) trusts, as `ed25519:` strings.
    fn trusted_by_this_build() -> Vec<String> {
        release_keys(&UpgradeSection::default())
            .unwrap()
            .iter()
            .map(encode_public_key)
            .collect()
    }

    #[cfg(not(feature = "nightly-signing"))]
    #[test]
    fn a_release_build_trusts_the_release_keys_and_never_a_nightly_key() {
        let trusted = trusted_by_this_build();
        assert_eq!(trusted, EMBEDDED_RELEASE_KEYS);
        for key in NIGHTLY_SIGNING_KEYS {
            assert!(!trusted.iter().any(|candidate| candidate == key));
        }
    }

    #[cfg(feature = "nightly-signing")]
    #[test]
    fn a_nightly_build_trusts_the_nightly_keys_and_never_a_release_key() {
        let trusted = trusted_by_this_build();
        assert_eq!(trusted, NIGHTLY_SIGNING_KEYS);
        for key in EMBEDDED_RELEASE_KEYS {
            assert!(!trusted.iter().any(|candidate| candidate == key));
        }
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

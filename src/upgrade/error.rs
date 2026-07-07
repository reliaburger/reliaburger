//! Error type for the upgrade subsystem.

use std::path::PathBuf;

/// Errors from the self-upgrade subsystem.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    /// The input could not be parsed as a semantic version.
    #[error("invalid version {input:?}: {reason}")]
    InvalidVersion { input: String, reason: String },

    /// A public or private key could not be parsed.
    #[error("invalid key {input:?}: {reason}")]
    InvalidKey { input: String, reason: String },

    /// Keypair generation failed (entropy or ring internal error).
    #[error("failed to generate an ed25519 keypair")]
    KeyGeneration,

    /// A signature envelope file was unreadable or malformed.
    #[error("invalid signature envelope {path}: {reason}")]
    InvalidEnvelope { path: PathBuf, reason: String },

    /// The binary's hash does not match its envelope.
    #[error("binary hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    /// The embedded signature does not verify against any release key.
    #[error("embedded signature does not verify against the release key set")]
    EmbeddedSignatureInvalid,

    /// The external signature is missing or does not verify.
    #[error("external signature is missing or does not verify")]
    ExternalSignatureInvalid,

    /// A network upgrade was attempted without an external signing key.
    #[error("network upgrades require upgrades.external_signing_key in node.toml")]
    ExternalKeyRequired,

    /// An upgrade marker file was unreadable or malformed.
    #[error("invalid upgrade marker {path}: {reason}")]
    InvalidMarker { path: PathBuf, reason: String },

    /// The requested version has no binary in the store.
    #[error("version {version} is not installed in the binary store")]
    UnknownVersion {
        version: crate::upgrade::version::BinaryVersion,
    },

    /// Another upgrade is already in flight on this node.
    #[error("upgrade {upgrade_id} is already in flight on this node")]
    AlreadyInFlight { upgrade_id: String },

    /// No older version is installed to roll back to.
    #[error("no older version installed to roll back to")]
    NoRollbackTarget,

    /// Downloading the binary failed.
    #[error("failed to fetch binary from {url}: {reason}")]
    FetchFailed { url: String, reason: String },

    /// `execv` of the new binary failed.
    #[error("exec failed: {reason}")]
    ExecFailed { reason: String },

    /// An underlying filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

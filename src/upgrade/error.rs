//! Error type for the upgrade subsystem.

use std::path::PathBuf;

/// Errors from the self-upgrade subsystem.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    /// A verified candidate cannot safely use this cluster's wire or state formats.
    #[error("incompatible binary: {0}")]
    IncompatibleBinary(String),

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

    /// A cluster upgrade was started without the operator's external
    /// signature. Every node fetches the binary from Pickle, so every node
    /// would refuse it.
    #[error(
        "cluster upgrades need an external signature: sign the binary with \
         `relish dev sign-binary --external-key` before starting"
    )]
    ExternalSignatureRequired,

    /// Some nodes have no external key to verify a cluster upgrade with.
    #[error(
        "{nodes} cannot accept a cluster upgrade: set upgrades.external_signing_key \
         in node.toml on every node first"
    )]
    NodesLackExternalKey {
        /// `node n1, node n2`.
        nodes: String,
    },

    /// `relish upgrade abort` on an upgrade that isn't paused.
    #[error("the upgrade is {phase}, not paused; only a paused upgrade can be aborted")]
    AbortNotPaused { phase: String },

    /// `relish upgrade abort` on a paused upgrade that already moved nodes.
    #[error(
        "{nodes} already moved to {target} (or may be mid-swap); aborting would leave \
         the cluster on mixed versions: roll back with `relish upgrade rollback <version>` instead"
    )]
    AbortWouldStrandNodes {
        /// `node n1, node n2`.
        nodes: String,
        target: crate::upgrade::version::BinaryVersion,
    },

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

    /// This node already attempted and reverted this exact upgrade.
    /// Retrying needs a fresh upgrade id (see `orchestrator::resume`).
    #[error("upgrade {upgrade_id} already failed and was reverted on this node")]
    PreviouslyFailed { upgrade_id: String },

    /// The node already runs the target version with exactly these bytes.
    /// Nothing to do; callers report it rather than treat it as a failure.
    #[error("{version} is already running with this exact binary; nothing to do")]
    AlreadyRunning {
        version: crate::upgrade::version::BinaryVersion,
    },

    /// The target version is already running but the candidate's bytes
    /// differ. The store and the rolling walk are keyed by version, so a
    /// same-version "upgrade" can never swap anything: refuse it loudly.
    #[error(
        "{node} already runs {version} but with a different binary \
         (running sha256 {running}, candidate sha256 {candidate}); \
         give the candidate a new version"
    )]
    SameVersionDifferentBinary {
        /// `node n1` or `this node`.
        node: String,
        version: crate::upgrade::version::BinaryVersion,
        running: String,
        candidate: String,
    },

    /// The target version is older than what the node runs and the caller
    /// did not ask for a downgrade.
    #[error(
        "{node} runs {running}, which is newer than {target}; \
         pass --allow-downgrade to install an older version (or use `relish upgrade rollback`)"
    )]
    DowngradeRefused {
        /// `node n1` or `this node`.
        node: String,
        running: crate::upgrade::version::BinaryVersion,
        target: crate::upgrade::version::BinaryVersion,
    },

    /// The store already holds different bytes under this version's name.
    #[error(
        "the binary store already holds a different {version} \
         (stored sha256 {stored}, incoming sha256 {incoming}); refusing to replace it"
    )]
    VersionContentConflict {
        version: crate::upgrade::version::BinaryVersion,
        stored: String,
        incoming: String,
    },

    /// No older version is installed to roll back to.
    #[error("no older version installed to roll back to")]
    NoRollbackTarget,

    /// Downloading the binary failed for good: the source answered, and
    /// the answer was no (a 4xx such as 404 for a blob it doesn't hold).
    #[error("failed to fetch binary from {url}: {reason}")]
    FetchFailed { url: String, reason: String },

    /// The binary's source could not serve it right now: the connection
    /// failed or timed out, or the registry answered 5xx, 408 or 429. A
    /// registry that is restarting looks exactly like this, so callers
    /// retry it rather than treat it as a refusal.
    #[error("binary source {url} is unavailable: {reason}")]
    FetchUnavailable { url: String, reason: String },

    /// Release metadata was unreadable or malformed.
    #[error("invalid release metadata: {reason}")]
    InvalidMetadata { reason: String },

    /// `execv` of the new binary failed.
    #[error("exec failed: {reason}")]
    ExecFailed { reason: String },

    /// An underlying filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl UpgradeError {
    /// Would the same request plausibly succeed if repeated shortly?
    ///
    /// Only an unavailable binary source is. Everything else (a signature
    /// or hash that doesn't verify, a missing external key, a version the
    /// policy refuses, a blob the registry says it doesn't have) gives the
    /// same answer every time, so retrying only delays the pause.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::FetchUnavailable { .. })
    }
}

//! Error type for the upgrade subsystem.

/// Errors from the self-upgrade subsystem.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    /// The input could not be parsed as a semantic version.
    #[error("invalid version {input:?}: {reason}")]
    InvalidVersion { input: String, reason: String },

    /// An underlying filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

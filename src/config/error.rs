/// Configuration errors.
///
/// Covers both parse-time failures (bad TOML, invalid resource strings)
/// and validation failures (missing required fields, conflicting options).
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse TOML: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("invalid resource value {value:?}: {reason}")]
    InvalidResourceValue { value: String, reason: String },

    #[error("invalid resource range {value:?}: {reason}")]
    InvalidResourceRange { value: String, reason: String },

    #[error("invalid replicas value: expected a positive integer or \"*\"")]
    InvalidReplicas,

    #[error("app {name:?} requires an image")]
    MissingImage { name: String },

    #[error("config file in app {name:?} must have exactly one of 'content' or 'source'")]
    InvalidConfigFile { name: String },

    #[error("invalid port {port} in app {name:?}: must be 1-65535")]
    InvalidPort { name: String, port: u16 },

    #[error("storage path {field:?} must be absolute, got {path:?}")]
    NonAbsolutePath { field: String, path: PathBuf },

    #[error("invalid port range {value:?}: {reason}")]
    InvalidPortRange { value: String, reason: String },

    #[error("volume in app {name:?}: {reason}")]
    InvalidVolume { name: String, reason: String },

    #[error("{field} in {context:?}: {reason}")]
    Validation {
        field: String,
        context: String,
        reason: String,
    },
}

/// Relish CLI library.
///
/// Separates CLI logic from the binary so it can be tested as a library.
/// The binary (`src/bin/relish.rs`) handles argument parsing and exit codes;
/// this module handles everything else.
pub mod chaos;
pub mod client;
pub mod commands;
pub mod dev;
pub mod output;
pub mod plan;

pub use output::OutputFormat;
pub use plan::{ApplyPlan, PlanAction, PlanEntry};

use crate::config::ConfigError;

/// Errors from Relish CLI operations.
#[derive(Debug, thiserror::Error)]
pub enum RelishError {
    /// Configuration parse or validation failure.
    #[error("{0}")]
    Config(#[from] ConfigError),

    /// JSON serialisation failure.
    #[error("failed to serialise JSON: {0}")]
    SerialiseJson(serde_json::Error),

    /// YAML serialisation failure.
    #[error("failed to serialise YAML: {0}")]
    SerialiseYaml(serde_yaml::Error),

    /// Command requires a running Bun agent.
    #[error("{command} requires a running Bun agent (not available in single-node mode yet)")]
    AgentRequired { command: String },

    /// The Bun agent is not reachable.
    #[error("bun agent not reachable at localhost:9117 (is it running?)")]
    AgentUnreachable,

    /// A request to the agent timed out. The operation may still be running.
    #[error("request timed out (the operation may still be running on the agent)")]
    RequestTimeout,

    /// The API returned an error.
    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },

    /// File already exists (init refuses to overwrite).
    #[error("{path} already exists (refusing to overwrite)")]
    FileExists { path: String },

    /// Lima (limactl) not found in PATH.
    #[error(
        "limactl not found — install Lima: brew install lima (macOS) or see https://lima-vm.io"
    )]
    LimaNotFound,

    /// Lima command failed.
    #[error("lima error ({command}): {stderr}")]
    LimaError { command: String, stderr: String },

    /// Dev cluster not found.
    #[error("dev cluster {name:?} not found — run `relish dev create` first")]
    DevClusterNotFound { name: String },

    /// Dev cluster already exists.
    #[error("dev cluster {name:?} already exists — destroy it first with `relish dev destroy`")]
    DevClusterAlreadyExists { name: String },

    /// IO error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

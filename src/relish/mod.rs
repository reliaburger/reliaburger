pub mod bench_cmd;
/// Relish CLI library.
///
/// Separates CLI logic from the binary so it can be tested as a library.
/// The binary (`src/bin/relish.rs`) handles argument parsing and exit codes;
/// this module handles everything else.
pub mod client;
pub mod command_reference;
pub mod commands;
pub mod compile;
pub mod dashboard;
pub mod dev;
pub mod diff;
pub mod fault;
pub mod fmt;
pub mod install;
#[cfg(feature = "kubernetes")]
#[allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::single_match
)]
pub mod k8s_export;
#[cfg(feature = "kubernetes")]
#[allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::single_match
)]
pub mod k8s_import;
pub mod local_context;
pub mod manifest;
pub mod manual;
pub mod metrics_cmd;
pub mod output;
pub mod path_cmd;
pub mod plan;
pub mod quickstart;
pub mod reader;
pub mod readiness;
pub mod setup;
pub mod source;
pub mod test_cmd;
mod tls;
pub mod tui;
pub mod uninstall;
pub mod upgrade;
pub mod wtf;
pub mod wtf_cmd;

pub use output::OutputFormat;
pub use plan::{ApplyPlan, PlanAction, PlanEntry};

use crate::config::ConfigError;

/// The result of a diagnostic-style command whose *exit code* carries meaning
/// beyond "did the tool itself error".
///
/// `relish test`, `wtf`, `bench` and `path` need to say three different
/// things a plain `Result<(), _>` cannot. An `Ok(())` collapses to exit 0 and
/// an `Err` to exit 1 — but "the suite ran and everything passed" and "the
/// suite ran and something failed" are both `Ok` as far as the *tool* is
/// concerned, and CI needs to tell them apart. So these commands return this
/// instead, and the binary maps it to a process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Ran fine, found nothing wrong. Exit 0.
    Clean,
    /// Ran fine, but found failures the caller should act on. Exit 1.
    Problems,
    /// Ran fine, found only warnings. Exit 2.
    Warnings,
}

impl CommandOutcome {
    /// The process exit code this outcome maps to.
    pub fn exit_code(self) -> u8 {
        match self {
            CommandOutcome::Clean => 0,
            CommandOutcome::Problems => 1,
            CommandOutcome::Warnings => 2,
        }
    }
}

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

    /// A WebSocket connection or frame failed.
    #[error("websocket: {0}")]
    WebSocket(String),

    /// A command-line flag value could not be parsed.
    #[error("invalid --{flag} value: {reason}")]
    InvalidFlag { flag: String, reason: String },

    /// A request to the agent timed out. The operation may still be running.
    #[error("request timed out (the operation may still be running on the agent)")]
    RequestTimeout,

    /// The API returned an error.
    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },

    /// Explicit refusal from the live scheduler's capacity admission check.
    #[error("scheduler refused placement: {0}")]
    SchedulingRejected(crate::meat::scheduler::ScheduleError),

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

    /// Cluster init (PKI generation) failed.
    #[error("cluster initialisation failed: {0}")]
    InitFailed(String),

    /// TOML formatting failed.
    #[error("format failed: {0}")]
    FormatFailed(String),

    /// A manifest URL could not be downloaded.
    #[error("failed to fetch manifest {url}: {reason}")]
    ManifestFetch { url: String, reason: String },

    /// IO error.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// `relish sign` was given an image the Pickle registry doesn't hold.
    #[error(
        "{image} is not in the cluster's Pickle registry (push it first; relish sign signs Pickle-hosted images only)"
    )]
    ImageNotInRegistry { image: String },

    /// Image signing key error (generation, parsing, signing).
    #[error("{0}")]
    ImageSigning(#[from] crate::pickle::signing::SigningError),

    /// Upgrade tooling error (key generation, signing).
    #[error("{0}")]
    Upgrade(#[from] crate::upgrade::error::UpgradeError),

    /// Joining the cluster failed (token rejected, member unreachable,
    /// fingerprint mismatch, or the identity could not be persisted).
    #[error("join failed: {0}")]
    JoinFailed(String),

    /// Council disaster recovery failed (12b.2 D21/CP12).
    #[error("council recover failed: {0}")]
    Recovery(String),

    /// `relish uninstall` refused or could not remove something.
    #[error("{0}")]
    Uninstall(#[from] uninstall::UninstallError),

    /// `relish manual CHAPTER` named no single chapter.
    #[error("{0}")]
    Manual(#[from] manual::ManualError),
}

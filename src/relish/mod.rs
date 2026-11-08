pub mod agent;
pub mod audit_cmd;
pub mod bench_cmd;
pub mod bench_jobs;
pub mod ca_cmd;
/// Relish CLI library.
///
/// Separates CLI logic from the binary so it can be tested as a library.
/// The binary (`src/bin/relish.rs`) handles argument parsing and exit codes;
/// this module handles everything else.
pub mod client;
pub mod command_reference;
pub mod commands;
pub mod compile;
pub mod concept_budget;
pub mod council_view;
pub mod dashboard;
pub mod dev;
pub mod diff;
pub mod events_cmd;
pub mod fault;
pub mod fmt;
pub mod inspect;
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
pub mod mcp;
pub mod metrics_cmd;
pub mod output;
pub mod path_cmd;
pub mod plan;
pub mod policy_cmd;
pub mod quickstart;
pub mod reader;
pub mod readiness;
pub mod recover_guard;
pub mod released_cli;
pub mod setup;
pub mod source;
pub mod source_bundle;
pub mod test_cmd;
mod tls;
pub mod tui;
pub mod uninstall;
pub mod unknown_command;
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
    /// Ran fine, found only warnings. Exit 3: clap already uses 2 for a
    /// usage error, and a script must be able to tell the two apart.
    Warnings,
    /// `relish exec` ran a command that exited with this code; relish exits
    /// with it too.
    Exited(u8),
}

impl CommandOutcome {
    /// The process exit code this outcome maps to.
    pub fn exit_code(self) -> u8 {
        match self {
            CommandOutcome::Clean => 0,
            CommandOutcome::Problems => 1,
            CommandOutcome::Warnings => 3,
            CommandOutcome::Exited(code) => code,
        }
    }
}

/// A command that takes over the whole terminal (#1569).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullScreen {
    /// `relish` with no subcommand, or `relish tui`.
    Tui,
    /// The `relish manual` reader.
    Manual,
    /// The `relish source` browser.
    Source,
}

impl FullScreen {
    /// Refuse at once unless stdin and stdout are both a terminal: the TUI
    /// reads keys in raw mode and draws on an alternate screen, and with no
    /// terminal it fails with an ioctl error or waits for keys that never
    /// come.
    pub fn require_terminal(self) -> Result<(), RelishError> {
        use std::io::IsTerminal;
        self.check_terminal(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    }

    fn check_terminal(self, interactive: bool) -> Result<(), RelishError> {
        if interactive {
            Ok(())
        } else {
            Err(RelishError::NoTerminal(self))
        }
    }

    /// How the command is named in a message.
    fn name(self) -> &'static str {
        match self {
            FullScreen::Tui => "the relish TUI",
            FullScreen::Manual => "relish manual",
            FullScreen::Source => "relish source",
        }
    }

    /// What to run instead from a shell with no terminal.
    fn instead(self) -> &'static str {
        match self {
            FullScreen::Tui => {
                "`relish agent guide` lists the commands that print plain output, such as `relish status`"
            }
            FullScreen::Manual => "`relish agent manual search` and `show` print it as Markdown",
            FullScreen::Source => "`relish agent source` searches and prints the source",
        }
    }
}

/// Why a connection to a Bun agent never carried a request. Each cause's
/// message ends with what to try, because "is it running?" was wrong for
/// most of them (#1567).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnreachableCause {
    /// The host name didn't resolve to an address.
    #[error(
        "the host name doesn't resolve; check the address in --endpoint, RELIABURGER_ENDPOINT or the current context"
    )]
    NameResolution,
    /// The host answered, and nothing listened on the port.
    #[error("connection refused; check that bun runs there and listens on that port")]
    Refused,
    /// The TLS handshake reached the server's certificate, and relish didn't
    /// trust it: an unknown CA, the wrong name, an expired certificate.
    #[error(
        "the server's certificate isn't trusted ({0}); pass the cluster CA with --ca-cert or RELIABURGER_CA_CERT"
    )]
    UntrustedCertificate(String),
    /// The TCP connection opened, and the TLS handshake failed for a reason
    /// other than the certificate.
    #[error("TLS handshake failed ({0}); check the endpoint is https and serves TLS")]
    Tls(String),
    /// Nothing answered before relish gave up.
    #[error(
        "no answer before the connect timeout; the host may be down, or a firewall may drop packets to that port"
    )]
    TimedOut,
    /// Anything else, as the network stack described it.
    #[error("{0}")]
    Other(String),
}

/// Errors from Relish CLI operations.
#[derive(Debug, thiserror::Error)]
pub enum RelishError {
    /// Configuration parse or validation failure.
    #[error("{0}")]
    Config(#[from] ConfigError),

    /// The embedded source bundle couldn't be read or extracted.
    #[error("{0}")]
    SourceBundle(#[from] source_bundle::SourceBundleError),

    /// JSON serialisation failure.
    #[error("failed to serialise JSON: {0}")]
    SerialiseJson(serde_json::Error),

    /// YAML serialisation failure.
    #[error("failed to serialise YAML: {0}")]
    SerialiseYaml(yaml_serde::Error),

    /// Command requires a running Bun agent.
    #[error("{command} requires a running Bun agent (not available in single-node mode yet)")]
    AgentRequired { command: String },

    /// No connection to the Bun agent carried the request: it names the
    /// endpoint relish dialled and why that failed (#1567).
    #[error("cannot reach the bun agent at {endpoint}: {cause}")]
    AgentUnreachable {
        /// The scheme, host and port relish dialled (`https://10.0.0.5:9117`).
        endpoint: String,
        /// What went wrong, with what to do about it.
        cause: UnreachableCause,
    },

    /// A full-screen command ran without a terminal (#1569).
    #[error("{} needs a terminal, and stdin or stdout isn't one; {}", .0.name(), .0.instead())]
    NoTerminal(FullScreen),

    /// A WebSocket connection or frame failed.
    #[error("websocket: {0}")]
    WebSocket(String),

    /// A command-line flag value could not be parsed.
    #[error("invalid --{flag} value: {reason}")]
    InvalidFlag { flag: String, reason: String },

    /// `relish upgrade plan` couldn't see the live cluster, so it won't
    /// guess its shape (#1568).
    #[error(
        "can't plan an upgrade for a cluster relish couldn't see ({0}); fix that, or pass --cluster-size N to plan for a cluster of N nodes"
    )]
    UnseenCluster(Box<RelishError>),

    /// A request to the agent timed out. The operation may still be running.
    #[error("request timed out (the operation may still be running on the agent)")]
    RequestTimeout,

    /// The API returned an error.
    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },

    /// The cluster's policies refused the apply, or would (#1117).
    #[error("the cluster's policies refuse this apply:\n  {}", denials.join("\n  "))]
    PolicyDenied {
        /// One line per denial, naming the policy and the app.
        denials: Vec<String>,
    },

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

    /// `relish images delete` takes a tag, never a digest or a bare name.
    #[error(
        "expected repository:tag (e.g. team/web:v1), got {reference:?}; an image leaves by losing its tags, never by digest"
    )]
    InvalidImageTag { reference: String },

    /// The agent reported no registry listener to send the request to.
    #[error("the agent reports no Pickle registry endpoint")]
    RegistryUnavailable,

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

    /// A chaos scenario was invalid, or one of its steps was refused or
    /// failed, so the command exits non-zero.
    #[error("scenario: {0}")]
    Scenario(String),

    /// `relish uninstall` refused or could not remove something.
    #[error("{0}")]
    Uninstall(#[from] uninstall::UninstallError),

    /// `relish manual CHAPTER` named no single chapter.
    #[error("{0}")]
    Manual(#[from] manual::ManualError),

    /// A root CA backup couldn't be built, sealed, opened or verified.
    #[error("{0}")]
    RootBackup(#[from] crate::sesame::root_backup::RootBackupError),

    /// The root couldn't sign the council's intermediate CSR (F04 R4).
    #[error("{0}")]
    CertificateAuthority(#[from] crate::sesame::ca::CaError),

    /// The master key or security state `relish init` wrote couldn't be read.
    #[error("{0}")]
    SecurityFiles(#[from] crate::sesame::bootstrap::BootstrapError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_screen_command_runs_on_a_terminal() {
        for command in [FullScreen::Tui, FullScreen::Manual, FullScreen::Source] {
            assert!(command.check_terminal(true).is_ok(), "{command:?}");
        }
    }

    /// Each refusal names the plain-output command to run instead.
    #[test]
    fn a_full_screen_command_without_a_terminal_names_what_to_run_instead() {
        let refusal = |command: FullScreen| command.check_terminal(false).unwrap_err().to_string();
        assert_eq!(
            refusal(FullScreen::Tui),
            "the relish TUI needs a terminal, and stdin or stdout isn't one; `relish agent guide` lists the commands that print plain output, such as `relish status`"
        );
        assert_eq!(
            refusal(FullScreen::Manual),
            "relish manual needs a terminal, and stdin or stdout isn't one; `relish agent manual search` and `show` print it as Markdown"
        );
        assert_eq!(
            refusal(FullScreen::Source),
            "relish source needs a terminal, and stdin or stdout isn't one; `relish agent source` searches and prints the source"
        );
    }
}

//! The `relish.agent/v1` envelope every `relish agent` command prints.
//!
//! One shape for every command, success or failure, so an agent learns it
//! once: which command answered, which build of `relish` answered it, and
//! then either the `data` with what to run next, or an `error` with a code
//! from a closed list. The types here are the contract; `relish agent
//! schema` prints their JSON Schema, generated from these same definitions.

use schemars::JsonSchema;
use serde::Serialize;

use crate::relish::{RelishError, UnreachableCause};

/// The envelope's version tag. One variant, so the schema says exactly which
/// string to expect; a `v2` would be a new variant and a deliberate change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum SchemaVersion {
    /// The first version, `relish.agent/v1`.
    #[serde(rename = "relish.agent/v1")]
    V1,
}

/// Every `relish agent` command, by the name the envelope's `command` field
/// carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    /// `relish agent` itself, when the arguments named no command it knows.
    #[value(skip)]
    Agent,
    /// `relish agent guide`.
    Guide,
    /// `relish agent version`.
    Version,
    /// `relish agent schema`.
    Schema,
    /// `relish agent source version`.
    #[serde(rename = "source version")]
    #[value(name = "source-version")]
    SourceVersion,
    /// `relish agent source tree`.
    #[serde(rename = "source tree")]
    #[value(name = "source-tree")]
    SourceTree,
    /// `relish agent source grep`.
    #[serde(rename = "source grep")]
    #[value(name = "source-grep")]
    SourceGrep,
    /// `relish agent source show`.
    #[serde(rename = "source show")]
    #[value(name = "source-show")]
    SourceShow,
    /// `relish agent source extract`.
    #[serde(rename = "source extract")]
    #[value(name = "source-extract")]
    SourceExtract,
    /// `relish agent logs`.
    Logs,
    /// `relish agent manual search`.
    #[serde(rename = "manual search")]
    #[value(name = "manual-search")]
    ManualSearch,
    /// `relish agent manual show`.
    #[serde(rename = "manual show")]
    #[value(name = "manual-show")]
    ManualShow,
    /// `relish agent context`.
    Context,
    /// `relish agent diagnose`.
    Diagnose,
    /// `relish agent explain`.
    Explain,
    /// `relish agent timeline`.
    Timeline,
    /// `relish agent upgrade-check`.
    #[serde(rename = "upgrade-check")]
    UpgradeCheck,
    /// `relish agent verify`.
    Verify,
    /// `relish agent report`.
    Report,
    /// `relish agent patch init`.
    #[serde(rename = "patch init")]
    #[value(name = "patch-init")]
    PatchInit,
    /// `relish agent patch build`.
    #[serde(rename = "patch build")]
    #[value(name = "patch-build")]
    PatchBuild,
    /// `relish agent patch test`.
    #[serde(rename = "patch test")]
    #[value(name = "patch-test")]
    PatchTest,
    /// `relish agent patch try`.
    #[serde(rename = "patch try")]
    #[value(name = "patch-try")]
    PatchTry,
    /// `relish agent patch diff`.
    #[serde(rename = "patch diff")]
    #[value(name = "patch-diff")]
    PatchDiff,
    /// `relish agent patch apply`.
    #[serde(rename = "patch apply")]
    #[value(name = "patch-apply")]
    PatchApply,
    /// `relish agent patch destroy`.
    #[serde(rename = "patch destroy")]
    #[value(name = "patch-destroy")]
    PatchDestroy,
    /// `relish agent setup`.
    Setup,
    /// `relish agent lease take`.
    #[serde(rename = "lease take")]
    #[value(name = "lease-take")]
    LeaseTake,
    /// `relish agent lease renew`.
    #[serde(rename = "lease renew")]
    #[value(name = "lease-renew")]
    LeaseRenew,
    /// `relish agent lease release`.
    #[serde(rename = "lease release")]
    #[value(name = "lease-release")]
    LeaseRelease,
    /// `relish agent propose`.
    Propose,
    /// `relish agent proposals`.
    Proposals,
    /// `relish agent withdraw`.
    Withdraw,
    /// `relish agent undo`.
    Undo,
}

impl CommandName {
    /// The command as a caller types it after `relish agent`.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandName::Agent => "agent",
            CommandName::Guide => "guide",
            CommandName::Version => "version",
            CommandName::Schema => "schema",
            CommandName::SourceVersion => "source version",
            CommandName::SourceTree => "source tree",
            CommandName::SourceGrep => "source grep",
            CommandName::SourceShow => "source show",
            CommandName::SourceExtract => "source extract",
            CommandName::Logs => "logs",
            CommandName::ManualSearch => "manual search",
            CommandName::ManualShow => "manual show",
            CommandName::Context => "context",
            CommandName::Diagnose => "diagnose",
            CommandName::Explain => "explain",
            CommandName::Timeline => "timeline",
            CommandName::UpgradeCheck => "upgrade-check",
            CommandName::Verify => "verify",
            CommandName::Report => "report",
            CommandName::PatchInit => "patch init",
            CommandName::PatchBuild => "patch build",
            CommandName::PatchTest => "patch test",
            CommandName::PatchTry => "patch try",
            CommandName::PatchDiff => "patch diff",
            CommandName::PatchApply => "patch apply",
            CommandName::PatchDestroy => "patch destroy",
            CommandName::Setup => "setup",
            CommandName::LeaseTake => "lease take",
            CommandName::LeaseRenew => "lease renew",
            CommandName::LeaseRelease => "lease release",
            CommandName::Propose => "propose",
            CommandName::Proposals => "proposals",
            CommandName::Withdraw => "withdraw",
            CommandName::Undo => "undo",
        }
    }

    /// The commands that answer with `data`, in the order `relish agent
    /// schema` lists them.
    pub const ANSWERING: &'static [CommandName] = &[
        CommandName::Guide,
        CommandName::Version,
        CommandName::Schema,
        CommandName::SourceVersion,
        CommandName::SourceTree,
        CommandName::SourceGrep,
        CommandName::SourceShow,
        CommandName::SourceExtract,
        CommandName::Logs,
        CommandName::ManualSearch,
        CommandName::ManualShow,
        CommandName::Context,
        CommandName::Diagnose,
        CommandName::Explain,
        CommandName::Timeline,
        CommandName::UpgradeCheck,
        CommandName::Verify,
        CommandName::Report,
        CommandName::PatchInit,
        CommandName::PatchBuild,
        CommandName::PatchTest,
        CommandName::PatchTry,
        CommandName::PatchDiff,
        CommandName::PatchApply,
        CommandName::PatchDestroy,
        CommandName::Setup,
        CommandName::LeaseTake,
        CommandName::LeaseRenew,
        CommandName::LeaseRelease,
        CommandName::Propose,
        CommandName::Proposals,
        CommandName::Withdraw,
        CommandName::Undo,
    ];
}

/// Which `relish` produced the answer.
///
/// Every field is always present; a value the build doesn't know is `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct BuildInfo {
    /// The Cargo package version, e.g. `0.2.7`.
    pub version: String,
    /// The full git commit the binary was built from, when the build knew it.
    pub commit: Option<String>,
    /// Whether the checkout had uncommitted changes in the bundled paths,
    /// when git could say.
    pub dirty: Option<bool>,
    /// `sha256:` digest of the source bundle, as `relish agent source
    /// version` reports it.
    pub source_digest: Option<String>,
}

impl BuildInfo {
    /// This binary's build information.
    ///
    /// A release build reads `dirty` and `source_digest` from what it
    /// embedded, for free. A dev build packs its checkout to work them out,
    /// and says `null` when the checkout has gone.
    pub fn current() -> Self {
        use crate::relish::source_bundle;
        BuildInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: crate::upgrade::version::build_commit().map(str::to_string),
            dirty: source_bundle::dirty(),
            source_digest: source_bundle::digest()
                .ok()
                .map(|digest| format!("sha256:{digest}")),
        }
    }
}

/// A command the agent could run next, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NextStep {
    /// The full command line, ready to run.
    pub run: String,
    /// Why it's worth running, in one line.
    pub why: String,
    /// `true` when running it changes nothing anywhere; `false` means a human
    /// should say yes first.
    pub read_only: bool,
}

/// Why a command couldn't answer. A closed list: a new code is a deliberate
/// change to the contract, with a snapshot to prove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The arguments were wrong: an unknown command or flag, a bad value.
    Usage,
    /// No cluster endpoint is configured, or the command needs a running
    /// agent where there is none.
    NoEndpoint,
    /// The endpoint refused or never answered the connection.
    Unreachable,
    /// The endpoint's host name didn't resolve.
    Dns,
    /// The endpoint's certificate isn't one this CLI trusts.
    TlsUntrusted,
    /// The request was sent but no answer came in time.
    Timeout,
    /// The API wanted a token, or rejected the one given (401).
    Unauthenticated,
    /// The token isn't allowed to do this, or a policy refused it (403).
    Forbidden,
    /// The thing asked about doesn't exist (404).
    NotFound,
    /// The request conflicts with the cluster's current state (409).
    Conflict,
    /// Any other answer from the API that wasn't a success.
    Api,
    /// This binary's source doesn't match what the question needs.
    SourceMismatch,
    /// Building a patched binary failed.
    BuildFailed,
    /// This command can't run on this operating system.
    UnsupportedPlatform,
    /// Something went wrong inside `relish` itself.
    Internal,
    /// Someone else holds a lease on what you asked for; `lease` says who,
    /// why and for how long.
    LeaseHeld,
    /// An app changed since the proposal saw it.
    Stale,
    /// A policy or built-in rule refused the proposal.
    Guardrail,
}

/// The process exit status of a `relish agent` command.
///
/// It says only whether the command answered, never what the answer was: a
/// cluster with three critical findings still exits 0, because the command
/// answered. The verdict lives in `data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// The command answered. Exit 0.
    Answered,
    /// The arguments were wrong. Exit 2, as clap's own usage errors.
    Usage,
    /// The cluster couldn't be reached or wouldn't authenticate us. Exit 3.
    Unreachable,
    /// The cluster understood and refused (403, a policy). Exit 4.
    Refused,
    /// Anything else. Exit 5.
    Failed,
}

impl ExitStatus {
    /// The process exit code.
    pub fn code(self) -> u8 {
        match self {
            ExitStatus::Answered => 0,
            ExitStatus::Usage => 2,
            ExitStatus::Unreachable => 3,
            ExitStatus::Refused => 4,
            ExitStatus::Failed => 5,
        }
    }
}

impl ErrorCode {
    /// The exit status a failure with this code ends with.
    pub fn exit_status(self) -> ExitStatus {
        match self {
            ErrorCode::Usage => ExitStatus::Usage,
            ErrorCode::NoEndpoint
            | ErrorCode::Unreachable
            | ErrorCode::Dns
            | ErrorCode::TlsUntrusted
            | ErrorCode::Timeout
            | ErrorCode::Unauthenticated => ExitStatus::Unreachable,
            ErrorCode::Forbidden | ErrorCode::LeaseHeld | ErrorCode::Guardrail => {
                ExitStatus::Refused
            }
            ErrorCode::NotFound
            | ErrorCode::Conflict
            | ErrorCode::Stale
            | ErrorCode::Api
            | ErrorCode::SourceMismatch
            | ErrorCode::BuildFailed
            | ErrorCode::UnsupportedPlatform
            | ErrorCode::Internal => ExitStatus::Failed,
        }
    }
}

/// What went wrong, for a command that couldn't answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AgentError {
    /// What kind of failure, from a closed list.
    pub code: ErrorCode,
    /// One line saying what happened.
    pub message: String,
    /// The cluster endpoint the command tried, when it tried one.
    pub endpoint: Option<String>,
    /// The HTTP status the API answered with, for `api` and the codes that
    /// come from a status.
    pub status: Option<u16>,
    /// What to do about it, when we know.
    pub hint: Option<String>,
    /// The lease in the way, for `lease_held`; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<Box<super::lease::LeaseLine>>,
}

impl AgentError {
    /// An error with just a code and a message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        AgentError {
            code,
            message: message.into(),
            endpoint: None,
            status: None,
            hint: None,
            lease: None,
        }
    }

    /// The same error, with a hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<&RelishError> for AgentError {
    /// Classify an error from the rest of `relish`.
    ///
    /// A connection that never carried a request splits by its cause (#1567):
    /// a name that doesn't resolve is `dns`, a server certificate relish
    /// doesn't trust is `tls_untrusted`, and the rest are `unreachable`.
    fn from(error: &RelishError) -> Self {
        let message = error.to_string();
        match error {
            RelishError::AgentUnreachable { endpoint, cause } => {
                let (code, hint) = match cause {
                    UnreachableCause::NameResolution => (
                        ErrorCode::Dns,
                        "check the host name in --endpoint, RELIABURGER_ENDPOINT or the current context",
                    ),
                    UnreachableCause::UntrustedCertificate(_) => (
                        ErrorCode::TlsUntrusted,
                        "pass the cluster CA with --ca-cert or RELIABURGER_CA_CERT",
                    ),
                    UnreachableCause::Refused
                    | UnreachableCause::Tls(_)
                    | UnreachableCause::TimedOut
                    | UnreachableCause::Other(_) => (
                        ErrorCode::Unreachable,
                        "start a cluster (relish setup --quickstart) or point at one with --endpoint",
                    ),
                };
                let mut agent_error = AgentError::new(code, message).with_hint(hint);
                agent_error.endpoint = Some(endpoint.clone());
                agent_error
            }
            RelishError::AgentRequired { .. } => AgentError::new(ErrorCode::NoEndpoint, message),
            RelishError::RequestTimeout => AgentError::new(ErrorCode::Timeout, message),
            RelishError::ApiError { status, .. } => {
                let code = match status {
                    401 => ErrorCode::Unauthenticated,
                    403 => ErrorCode::Forbidden,
                    404 => ErrorCode::NotFound,
                    409 => ErrorCode::Conflict,
                    _ => ErrorCode::Api,
                };
                let mut agent_error = AgentError::new(code, message);
                // Status 0 is the client's own "couldn't parse the answer".
                agent_error.status = (*status != 0).then_some(*status);
                if code == ErrorCode::Unauthenticated {
                    agent_error = agent_error.with_hint(
                        "pass a token with --token or RELIABURGER_TOKEN; a read-only one is enough",
                    );
                }
                agent_error
            }
            RelishError::PolicyDenied { .. } => AgentError::new(ErrorCode::Forbidden, message),
            RelishError::InvalidFlag { .. } | RelishError::InvalidImageTag { .. } => {
                AgentError::new(ErrorCode::Usage, message)
            }
            RelishError::Manual(_)
            | RelishError::DevClusterNotFound { .. }
            | RelishError::ImageNotInRegistry { .. } => {
                AgentError::new(ErrorCode::NotFound, message)
            }
            RelishError::FileExists { .. } | RelishError::DevClusterAlreadyExists { .. } => {
                AgentError::new(ErrorCode::Conflict, message)
            }
            RelishError::LimaNotFound => AgentError::new(ErrorCode::UnsupportedPlatform, message),
            _ => AgentError::new(ErrorCode::Internal, message),
        }
    }
}

/// How a command ended: an answer, or a failure.
///
/// `untagged` means serde writes the variant's fields with no wrapper
/// naming the variant, and `flatten` on [`Envelope::outcome`] lifts them
/// into the envelope itself: an answer adds `data`, `truncated`, `next` and
/// `warnings`; a failure adds `error`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Outcome<T> {
    /// The command answered.
    Answered {
        /// The answer; its shape depends on the command.
        data: T,
        /// Whether the byte budget cut something out of `data`.
        truncated: bool,
        /// Commands worth running next.
        next: Vec<NextStep>,
        /// Things the agent should know that aren't the answer.
        warnings: Vec<String>,
    },
    /// The command couldn't answer.
    Failed {
        /// What went wrong.
        error: AgentError,
    },
}

/// The `relish.agent/v1` envelope every `relish agent` command prints:
/// `data` when `ok` is `true`, `error` when it's `false`.
// The doc comments on these types become the schema's descriptions, so they
// speak to the agent reading the schema. For the Rust reader: the fields are
// private, so `Envelope::answered` and `Envelope::failed` are the only ways
// to build one, and they keep `ok` and the outcome in step.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Envelope<T> {
    /// Always `relish.agent/v1`.
    schema: SchemaVersion,
    /// The command that answered.
    command: CommandName,
    /// `true` when the command answered and `data` is present; `false` when
    /// it couldn't and `error` is present.
    ok: bool,
    /// The build of `relish` that answered.
    relish: BuildInfo,
    #[serde(flatten)]
    outcome: Outcome<T>,
}

impl<T> Envelope<T> {
    /// A command's answer.
    pub fn answered(command: CommandName, relish: BuildInfo, data: T) -> Self {
        Envelope {
            schema: SchemaVersion::V1,
            command,
            ok: true,
            relish,
            outcome: Outcome::Answered {
                data,
                truncated: false,
                next: Vec::new(),
                warnings: Vec::new(),
            },
        }
    }

    /// A command that couldn't answer.
    pub fn failed(command: CommandName, relish: BuildInfo, error: AgentError) -> Self {
        Envelope {
            schema: SchemaVersion::V1,
            command,
            ok: false,
            relish,
            outcome: Outcome::Failed { error },
        }
    }

    /// The same answer, with a next step appended. A failure ignores it.
    pub fn with_next(mut self, step: NextStep) -> Self {
        if let Outcome::Answered { next, .. } = &mut self.outcome {
            next.push(step);
        }
        self
    }

    /// The same answer, with a warning appended. A failure ignores it.
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        if let Outcome::Answered { warnings, .. } = &mut self.outcome {
            warnings.push(warning.into());
        }
        self
    }

    /// The same answer, marked `truncated`: the command left part of `data`
    /// out to fit the byte budget, and says what in `data`. A failure
    /// ignores it.
    pub fn mark_truncated(mut self) -> Self {
        if let Outcome::Answered { truncated, .. } = &mut self.outcome {
            *truncated = true;
        }
        self
    }

    /// The command that answered.
    pub fn command(&self) -> CommandName {
        self.command
    }

    /// The build of `relish` that answered.
    pub fn relish(&self) -> &BuildInfo {
        &self.relish
    }

    /// The exit status this envelope ends the process with.
    pub fn exit_status(&self) -> ExitStatus {
        match &self.outcome {
            Outcome::Answered { .. } => ExitStatus::Answered,
            Outcome::Failed { error } => error.code.exit_status(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_build() -> BuildInfo {
        BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: None,
            source_digest: None,
        }
    }

    #[test]
    fn error_codes_serialise_in_snake_case() {
        let codes = [
            ErrorCode::Usage,
            ErrorCode::NoEndpoint,
            ErrorCode::Unreachable,
            ErrorCode::Dns,
            ErrorCode::TlsUntrusted,
            ErrorCode::Timeout,
            ErrorCode::Unauthenticated,
            ErrorCode::Forbidden,
            ErrorCode::NotFound,
            ErrorCode::Conflict,
            ErrorCode::Api,
            ErrorCode::SourceMismatch,
            ErrorCode::BuildFailed,
            ErrorCode::UnsupportedPlatform,
            ErrorCode::Internal,
        ];
        let names: Vec<String> = codes
            .iter()
            .map(|code| {
                serde_json::to_value(code)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "usage",
                "no_endpoint",
                "unreachable",
                "dns",
                "tls_untrusted",
                "timeout",
                "unauthenticated",
                "forbidden",
                "not_found",
                "conflict",
                "api",
                "source_mismatch",
                "build_failed",
                "unsupported_platform",
                "internal",
            ]
        );
    }

    #[test]
    fn exit_codes_say_only_whether_the_command_answered() {
        let answered = Envelope::answered(
            CommandName::Version,
            fixture_build(),
            serde_json::json!({"critical": 3}),
        );
        assert_eq!(answered.exit_status().code(), 0);
        let cases = [
            (ErrorCode::Usage, 2),
            (ErrorCode::NoEndpoint, 3),
            (ErrorCode::Unreachable, 3),
            (ErrorCode::Dns, 3),
            (ErrorCode::TlsUntrusted, 3),
            (ErrorCode::Timeout, 3),
            (ErrorCode::Unauthenticated, 3),
            (ErrorCode::Forbidden, 4),
            (ErrorCode::NotFound, 5),
            (ErrorCode::Conflict, 5),
            (ErrorCode::Api, 5),
            (ErrorCode::SourceMismatch, 5),
            (ErrorCode::BuildFailed, 5),
            (ErrorCode::UnsupportedPlatform, 5),
            (ErrorCode::Internal, 5),
        ];
        for (code, exit) in cases {
            let failed: Envelope<()> = Envelope::failed(
                CommandName::Version,
                fixture_build(),
                AgentError::new(code, "boom"),
            );
            assert_eq!(failed.exit_status().code(), exit, "{code:?}");
        }
    }

    #[test]
    fn relish_errors_map_to_their_codes() {
        let cases = [
            (
                RelishError::AgentUnreachable {
                    endpoint: "https://10.0.0.5:9117".to_string(),
                    cause: crate::relish::UnreachableCause::Refused,
                },
                ErrorCode::Unreachable,
            ),
            (
                RelishError::AgentUnreachable {
                    endpoint: "https://no-such-node.invalid:9117".to_string(),
                    cause: crate::relish::UnreachableCause::NameResolution,
                },
                ErrorCode::Dns,
            ),
            (
                RelishError::AgentUnreachable {
                    endpoint: "https://10.0.0.5:9117".to_string(),
                    cause: crate::relish::UnreachableCause::UntrustedCertificate(
                        "invalid peer certificate: UnknownIssuer".to_string(),
                    ),
                },
                ErrorCode::TlsUntrusted,
            ),
            (
                RelishError::AgentUnreachable {
                    endpoint: "https://10.0.0.5:9117".to_string(),
                    cause: crate::relish::UnreachableCause::Tls(
                        "received corrupt message".to_string(),
                    ),
                },
                ErrorCode::Unreachable,
            ),
            (RelishError::RequestTimeout, ErrorCode::Timeout),
            (
                RelishError::AgentRequired {
                    command: "logs".to_string(),
                },
                ErrorCode::NoEndpoint,
            ),
            (api(401), ErrorCode::Unauthenticated),
            (api(403), ErrorCode::Forbidden),
            (api(404), ErrorCode::NotFound),
            (api(409), ErrorCode::Conflict),
            (api(500), ErrorCode::Api),
            (
                RelishError::PolicyDenied {
                    denials: vec!["no-latest: web".to_string()],
                },
                ErrorCode::Forbidden,
            ),
            (
                RelishError::InvalidFlag {
                    flag: "since".to_string(),
                    reason: "not a duration".to_string(),
                },
                ErrorCode::Usage,
            ),
            (RelishError::LimaNotFound, ErrorCode::UnsupportedPlatform),
            (
                RelishError::Io(std::io::Error::other("disk on fire")),
                ErrorCode::Internal,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(AgentError::from(&error).code, code, "{error}");
        }
    }

    #[test]
    fn api_errors_carry_their_status_but_not_the_parse_sentinel() {
        assert_eq!(AgentError::from(&api(500)).status, Some(500));
        assert_eq!(AgentError::from(&api(0)).status, None);
    }

    fn api(status: u16) -> RelishError {
        RelishError::ApiError {
            status,
            body: "nope".to_string(),
        }
    }
}

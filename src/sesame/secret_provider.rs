//! Exec secret providers (#1123).
//!
//! An environment value such as `PROVIDER[vault:secret/db#password]` names a
//! secret that lives outside the cluster. When an instance launches, its node
//! runs the provider's binary, allowlisted in `node.toml` under
//! `[secret_providers.<name>]`, writes one JSON request to its stdin and reads
//! one JSON answer from its stdout. That's the credential-helper protocol git
//! and Docker use, so no vendor SDK is linked into Bun.
//!
//! Every call is bounded: a timeout, after which the provider's whole process
//! group is killed, and a cap on how much it may write. Anything else, from a
//! missing binary to an answer that isn't the protocol, is an error that
//! fails the instance's launch closed.
//!
//! Nothing here logs, and no error carries a secret value or anything the
//! provider wrote, apart from the short message it puts in its `error` field.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// What an environment value that names a provider starts with.
pub const PROVIDER_PREFIX: &str = "PROVIDER[";
const PROVIDER_SUFFIX: &str = "]";

/// The protocol version this node speaks, in every request and answer.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long a provider may take when `timeout_secs` isn't set.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// The longest `timeout_secs` a node accepts. A launch waits on the provider,
/// so a provider that needs longer is a provider to fix.
pub const MAX_TIMEOUT_SECS: u64 = 60;
/// How much a provider may write when `max_output_bytes` isn't set.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// The largest `max_output_bytes` a node accepts.
pub const MAX_OUTPUT_BYTES_CEILING: usize = 1024 * 1024;

/// The longest path inside `PROVIDER[...]`.
const MAX_PATH_BYTES: usize = 1024;
/// How long Bun keeps reading a provider's stdout after it has exited.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(100);
/// How much of a provider's own `error` message a launch failure keeps.
const MAX_PROVIDER_MESSAGE_CHARS: usize = 200;
/// The provider's `PATH` unless its `env` sets one: Bun's own environment is
/// never passed on.
const PROVIDER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------
// Names and references
// ---------------------------------------------------------------------------

/// The name of a secret provider: the `vault` in `PROVIDER[vault:...]` and
/// in `[secret_providers.vault]`. A lowercase DNS label.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderName(String);

impl ProviderName {
    /// The name as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderName {
    type Error = ReferenceError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if crate::config::valid_workload_label(&name) {
            Ok(Self(name))
        } else {
            Err(ReferenceError::InvalidProviderName(name))
        }
    }
}

impl From<ProviderName> for String {
    fn from(name: ProviderName) -> Self {
        name.0
    }
}

impl std::fmt::Display for ProviderName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a `PROVIDER[...]` value doesn't parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    /// The value doesn't end with `]`.
    #[error("a secret provider reference must look like PROVIDER[name:path] and end with \"]\"")]
    Unterminated,
    /// No `:` between the provider's name and the path.
    #[error("a secret provider reference must look like PROVIDER[name:path], with a \":\"")]
    MissingSeparator,
    /// The provider's name isn't a lowercase DNS label.
    #[error(
        "secret provider name {0:?} must be a lowercase DNS label of 1–63 bytes, with letters or digits at both ends and only letters, digits or hyphens within"
    )]
    InvalidProviderName(String),
    /// Nothing after the `:`.
    #[error("a secret provider reference needs a path after the provider's name")]
    EmptyPath,
    /// The path is longer than [`MAX_PATH_BYTES`].
    #[error("a secret provider path may be at most {MAX_PATH_BYTES} bytes")]
    PathTooLong,
    /// The path holds a control character, such as a newline.
    #[error("a secret provider path may not contain control characters")]
    ControlCharacter,
}

/// A parsed `PROVIDER[name:path]` value: which provider to ask, and the path
/// it's asked for. The path means whatever the provider says it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretReference {
    raw: String,
    provider: ProviderName,
    path: String,
}

impl SecretReference {
    /// Whether `value` names a provider at all, well formed or not.
    pub fn is_reference(value: &str) -> bool {
        value.starts_with(PROVIDER_PREFIX)
    }

    /// Parse `PROVIDER[name:path]`. The path runs from the first `:` to the
    /// final `]`, so it may itself hold `:` or `]`.
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        let inner = value
            .strip_prefix(PROVIDER_PREFIX)
            .and_then(|rest| rest.strip_suffix(PROVIDER_SUFFIX))
            .ok_or(ReferenceError::Unterminated)?;
        let (name, path) = inner
            .split_once(':')
            .ok_or(ReferenceError::MissingSeparator)?;
        let provider = ProviderName::try_from(name.to_string())?;
        if path.is_empty() {
            return Err(ReferenceError::EmptyPath);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(ReferenceError::PathTooLong);
        }
        if path.chars().any(char::is_control) {
            return Err(ReferenceError::ControlCharacter);
        }
        Ok(Self {
            raw: value.to_string(),
            provider,
            path: path.to_string(),
        })
    }

    /// The provider to ask.
    pub fn provider(&self) -> &ProviderName {
        &self.provider
    }

    /// What to ask it for.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The value as written, `PROVIDER[...]` and all.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

// ---------------------------------------------------------------------------
// Node configuration
// ---------------------------------------------------------------------------

/// One `[secret_providers.<name>]` table in `node.toml`: the binary a node
/// runs for that provider, and the bounds on each call.
///
/// The table is the allowlist. A node runs no provider it doesn't name, and
/// it advertises the names so the scheduler only places an app that needs a
/// provider on nodes that have it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretProviderConfig {
    /// Absolute path of the provider's binary.
    pub binary: PathBuf,
    /// How long one call may take, in seconds, before the provider is
    /// killed and the launch fails. Default 5, at most 60.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// The most a provider may write to stdout. Default 64 KiB, at most 1 MiB.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    /// The provider's whole environment, beside a default `PATH`. Bun's own
    /// environment is never passed on.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_max_output_bytes() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

impl SecretProviderConfig {
    /// A provider running `binary` with the default bounds.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            env: BTreeMap::new(),
        }
    }

    /// Why this table can't be used, if it can't.
    pub fn validate(&self) -> Result<(), String> {
        if !self.binary.is_absolute() {
            return Err(format!(
                "binary {} must be an absolute path",
                self.binary.display()
            ));
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(format!(
                "timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}, got {}",
                self.timeout_secs
            ));
        }
        if !(1..=MAX_OUTPUT_BYTES_CEILING).contains(&self.max_output_bytes) {
            return Err(format!(
                "max_output_bytes must be between 1 and {MAX_OUTPUT_BYTES_CEILING}, got {}",
                self.max_output_bytes
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a provider gave no secret. No variant carries the secret, or anything
/// the provider wrote apart from its own short `error` message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The value names a provider that doesn't parse.
    #[error(transparent)]
    InvalidReference(#[from] ReferenceError),
    /// The node has no `[secret_providers.<name>]` table for it.
    #[error("secret provider \"{provider}\" is not configured on this node")]
    NotConfigured { provider: ProviderName },
    /// The binary couldn't be run.
    #[error("secret provider \"{provider}\" could not run {}: {reason}", binary.display())]
    Spawn {
        provider: ProviderName,
        binary: PathBuf,
        reason: String,
    },
    /// Talking to it over its pipes failed.
    #[error("secret provider \"{provider}\" failed while answering: {reason}")]
    Io {
        provider: ProviderName,
        reason: String,
    },
    /// It didn't finish within its timeout, and was killed.
    #[error("secret provider \"{provider}\" did not answer within {}s and was killed", timeout.as_secs())]
    TimedOut {
        provider: ProviderName,
        timeout: Duration,
    },
    /// It wrote more than `max_output_bytes`, and was killed.
    #[error("secret provider \"{provider}\" wrote more than {limit} bytes and was killed")]
    OutputTooLarge {
        provider: ProviderName,
        limit: usize,
    },
    /// It exited unsuccessfully without a protocol `error` answer.
    #[error("secret provider \"{provider}\" {status}")]
    Exited {
        provider: ProviderName,
        status: String,
    },
    /// Its stdout isn't a protocol answer.
    #[error(
        "secret provider \"{provider}\" answered with something other than the protocol: {reason}"
    )]
    Malformed {
        provider: ProviderName,
        reason: &'static str,
    },
    /// It answered in a protocol version this node doesn't speak.
    #[error(
        "secret provider \"{provider}\" answered in protocol version {version}, and this node speaks version {PROTOCOL_VERSION}"
    )]
    UnsupportedVersion {
        provider: ProviderName,
        version: u32,
    },
    /// It answered with an `error`.
    #[error("secret provider \"{provider}\" refused: {message}")]
    Refused {
        provider: ProviderName,
        message: String,
    },
    /// The value it returned can't go in an environment variable.
    #[error("secret provider \"{provider}\" returned a value containing a NUL byte")]
    InvalidValue { provider: ProviderName },
}

/// A provider failure, with the environment variable it was resolving.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot resolve secret {variable}: {problem}")]
pub struct SecretResolutionError {
    /// The environment variable whose value named the provider.
    pub variable: String,
    /// What went wrong.
    pub problem: ProviderError,
}

// ---------------------------------------------------------------------------
// The protocol
// ---------------------------------------------------------------------------

/// Which instance a secret is resolved for. The provider gets all of it, so
/// it can decide which apps may read which paths.
#[derive(Debug, Clone, Copy)]
pub struct LaunchContext<'a> {
    pub namespace: &'a str,
    pub app: &'a str,
    pub instance: &'a str,
}

/// The one JSON object a provider reads from stdin.
#[derive(Serialize)]
struct ProviderRequest<'a> {
    version: u32,
    provider: &'a str,
    path: &'a str,
    namespace: &'a str,
    app: &'a str,
    instance: &'a str,
    variable: &'a str,
}

/// The one JSON object a provider writes to stdout: exactly one of `value`
/// and `error`. Deliberately not `Debug`, so the secret can't be printed.
#[derive(Deserialize)]
struct ProviderResponse {
    version: u32,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Turn what a provider wrote, and how it exited, into its secret.
fn interpret(
    provider: &ProviderName,
    status: std::process::ExitStatus,
    output: &[u8],
) -> Result<String, ProviderError> {
    // A parse error's message can quote the input, which is the secret, so
    // only whether it parsed is kept.
    let response = serde_json::from_slice::<ProviderResponse>(output).ok();
    let Some(response) = response else {
        if !status.success() {
            return Err(ProviderError::Exited {
                provider: provider.clone(),
                status: describe_exit(status),
            });
        }
        return Err(ProviderError::Malformed {
            provider: provider.clone(),
            reason: "stdout must be one JSON object with a version and a value or an error",
        });
    };
    if response.version != PROTOCOL_VERSION {
        return Err(ProviderError::UnsupportedVersion {
            provider: provider.clone(),
            version: response.version,
        });
    }
    match (response.value, response.error) {
        (_, Some(message)) => Err(ProviderError::Refused {
            provider: provider.clone(),
            message: shorten(&message),
        }),
        (Some(_), None) if !status.success() => Err(ProviderError::Exited {
            provider: provider.clone(),
            status: describe_exit(status),
        }),
        (Some(value), None) if value.contains('\0') => Err(ProviderError::InvalidValue {
            provider: provider.clone(),
        }),
        (Some(value), None) => Ok(value),
        (None, None) => Err(ProviderError::Malformed {
            provider: provider.clone(),
            reason: "the answer has neither a value nor an error",
        }),
    }
}

/// A provider's own error message on one short line.
fn shorten(message: &str) -> String {
    let line: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_PROVIDER_MESSAGE_CHARS)
        .collect();
    if message.chars().count() > MAX_PROVIDER_MESSAGE_CHARS {
        format!("{line}…")
    } else {
        line
    }
}

/// "exited with status 3" or "was killed by signal 9".
fn describe_exit(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with status {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("was killed by signal {signal}");
        }
    }
    "exited unsuccessfully".to_string()
}

/// How one exchange with a provider ended, short of the timeout.
enum Exchange {
    Finished(std::process::ExitStatus, Vec<u8>),
    TooLarge,
}

/// Write the request, read at most `limit + 1` bytes of the answer, and wait
/// for the provider to exit.
async fn exchange(
    child: &mut tokio::process::Child,
    request: &[u8],
    limit: usize,
) -> std::io::Result<Exchange> {
    if let Some(mut stdin) = child.stdin.take() {
        // A provider that never reads its request may close the pipe first.
        // Its answer still decides the outcome.
        match stdin.write_all(request).await {
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => other?,
        }
        drop(stdin);
    }
    let Some(stdout) = child.stdout.take() else {
        let status = child.wait().await?;
        return Ok(Exchange::Finished(status, Vec::new()));
    };
    let mut output = Vec::new();
    let exited = {
        // `take` stops reading one byte past the cap, so a provider that
        // streams forever can't fill Bun's memory.
        let mut limited = stdout.take(limit as u64 + 1);
        let read = limited.read_to_end(&mut output);
        tokio::pin!(read);
        tokio::select! {
            result = &mut read => {
                result?;
                None
            }
            status = child.wait() => {
                // The answer is complete when the provider exits, and
                // whatever it wrote is already in the pipe. End-of-file may
                // never come: a child the provider left behind, or a process
                // Bun spawned at the same moment, can still hold the write
                // end. So the rest gets a short grace, not a wait.
                if let Ok(result) = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut read).await {
                    result?;
                }
                Some(status?)
            }
        }
    };
    if output.len() > limit {
        return Ok(Exchange::TooLarge);
    }
    let status = match exited {
        Some(status) => status,
        None => child.wait().await?,
    };
    Ok(Exchange::Finished(status, output))
}

/// Kill a provider and everything it started. It runs as the leader of its
/// own process group, so a shell script's `sleep` dies with it.
async fn kill_provider(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(group) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // The child isn't reaped yet, so the group id can't have been reused.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill().await;
}

// ---------------------------------------------------------------------------
// The node's providers
// ---------------------------------------------------------------------------

/// The providers a node runs, from its `[secret_providers.*]` tables. Cheap
/// to clone: launches on several tasks share one map.
#[derive(Debug, Clone, Default)]
pub struct SecretProviders {
    providers: Arc<BTreeMap<ProviderName, SecretProviderConfig>>,
}

impl SecretProviders {
    /// The providers named in `node.toml`.
    pub fn new(providers: BTreeMap<ProviderName, SecretProviderConfig>) -> Self {
        Self {
            providers: Arc::new(providers),
        }
    }

    /// The names this node advertises to the scheduler.
    pub fn names(&self) -> BTreeSet<ProviderName> {
        self.providers.keys().cloned().collect()
    }

    /// Ask `reference`'s provider for its secret, for `variable` of the
    /// instance in `context`.
    pub async fn resolve(
        &self,
        reference: &SecretReference,
        variable: &str,
        context: LaunchContext<'_>,
    ) -> Result<String, ProviderError> {
        let provider = reference.provider();
        let config = self
            .providers
            .get(provider)
            .ok_or_else(|| ProviderError::NotConfigured {
                provider: provider.clone(),
            })?;
        let mut request = serde_json::to_vec(&ProviderRequest {
            version: PROTOCOL_VERSION,
            provider: provider.as_str(),
            path: reference.path(),
            namespace: context.namespace,
            app: context.app,
            instance: context.instance,
            variable,
        })
        .map_err(|error| ProviderError::Io {
            provider: provider.clone(),
            reason: error.to_string(),
        })?;
        // One line: a provider can read up to the newline and answer, without
        // waiting for end-of-file on stdin.
        request.push(b'\n');

        let mut command = tokio::process::Command::new(&config.binary);
        command
            .env_clear()
            .env("PATH", PROVIDER_PATH)
            .envs(&config.env)
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // A provider's stderr is its own diagnostics, and may hold
            // anything, the secret included. Bun neither logs nor keeps it.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| ProviderError::Spawn {
            provider: provider.clone(),
            binary: config.binary.clone(),
            reason: error.to_string(),
        })?;

        let timeout = Duration::from_secs(config.timeout_secs);
        let limit = config.max_output_bytes;
        let outcome = tokio::time::timeout(timeout, exchange(&mut child, &request, limit)).await;
        let failure = match outcome {
            Ok(Ok(Exchange::Finished(status, output))) => {
                return interpret(provider, status, &output);
            }
            Ok(Ok(Exchange::TooLarge)) => ProviderError::OutputTooLarge {
                provider: provider.clone(),
                limit,
            },
            Ok(Err(error)) => ProviderError::Io {
                provider: provider.clone(),
                reason: error.kind().to_string(),
            },
            Err(_) => ProviderError::TimedOut {
                provider: provider.clone(),
                timeout,
            },
        };
        kill_provider(&mut child).await;
        Err(failure)
    }

    /// Resolve every `KEY=PROVIDER[...]` entry of a launch's environment,
    /// one provider call each, in order. Other entries are kept as they are.
    /// The first failure stops the launch.
    pub async fn resolve_env(
        &self,
        env: &[String],
        context: LaunchContext<'_>,
    ) -> Result<Vec<String>, SecretResolutionError> {
        let mut resolved = Vec::with_capacity(env.len());
        for entry in env {
            let Some((variable, value)) = entry
                .split_once('=')
                .filter(|(_, value)| SecretReference::is_reference(value))
            else {
                resolved.push(entry.clone());
                continue;
            };
            let fail = |problem| SecretResolutionError {
                variable: variable.to_string(),
                problem,
            };
            let reference = SecretReference::parse(value).map_err(|e| fail(e.into()))?;
            let secret = self
                .resolve(&reference, variable, context)
                .await
                .map_err(fail)?;
            resolved.push(format!("{variable}={secret}"));
        }
        Ok(resolved)
    }
}

/// Whether a launch's environment names any provider.
pub fn needs_resolution(env: &[String]) -> bool {
    env.iter().any(|entry| {
        entry
            .split_once('=')
            .is_some_and(|(_, value)| SecretReference::is_reference(value))
    })
}

#[cfg(test)]
mod tests;

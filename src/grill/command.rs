//! Durable external commands for runtime lifecycle operations.
//!
//! Each collection belongs to one runtime generation. Its caller must retain
//! exclusive lifecycle ownership while registering commands and collecting
//! their retirement evidence. An inventory is not a fence against concurrent
//! registration. Timeouts retain command ownership; they never prove absence.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use super::InstanceId;
use super::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use super::process_control::ProcessControl;
use super::process_owner::OwnerPhase;

mod executor;
pub use executor::{ClaimedCommandExecutor, DirectCommandExecutor, RuntimeCommandExecutor};

const OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Identity of one immutable command attempt, independent of its caller's PID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(InstanceId);

/// Positive evidence currently available for a command generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandState {
    /// Intent exists, but no command has been authorised to execute.
    Prepared,
    /// Preparation was fenced before execution.
    Cancelled,
    /// The independent owner controls this command and its descendants.
    Running {
        /// Informational PID, never authority to send a recovered signal.
        pid: u32,
    },
    /// The owner confirmed the command and its supported descendants absent.
    Retired {
        /// Actual command exit code; signals have no ordinary exit code.
        exit_code: Option<i32>,
    },
}

/// Captured command output, available only after confirmed retirement.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Actual command exit code, or none for signal termination.
    pub exit_code: Option<i32>,
    /// Unmodified standard-output bytes.
    pub stdout: Vec<u8>,
    /// Unmodified standard-error bytes.
    pub stderr: Vec<u8>,
}

/// Failure to run, observe or retire an owned command.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// Filesystem or owner-control evidence was unavailable.
    #[error("runtime command failed: {0}")]
    Io(#[from] io::Error),
    /// The deadline expired, leaving the command's ownership record intact.
    #[error("runtime command {command:?} did not finish within {timeout:?}")]
    TimedOut {
        /// Attempt that still needs positive retirement evidence.
        command: CommandId,
        /// Maximum caller wait.
        timeout: Duration,
    },
    /// Preparation was cancelled without executing a command.
    #[error("runtime command was cancelled before execution")]
    Cancelled,
    /// The command retired, but its output cannot be returned within the bound.
    #[error("runtime command output exceeds 1 MiB limit")]
    OutputTooLarge,
}

/// Durable command attempts belonging to one exclusive runtime lifecycle.
#[derive(Debug, Clone)]
pub struct OwnedCommands {
    control: ProcessControl,
}

impl OwnedCommands {
    /// Open a command collection using the hidden owner in the given Bun binary.
    pub fn new(directory: PathBuf, executable: PathBuf) -> Self {
        Self {
            control: ProcessControl::new(directory.join("commands"), executable),
        }
    }

    /// Persist immutable input before returning permission to start an attempt.
    /// Caller cancellation can leave a discoverable Prepared command, never an
    /// unrecorded execution. Paths must be representable without lossy encoding.
    pub async fn prepare(
        &self,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
    ) -> Result<CommandId, CommandError> {
        let program = program
            .to_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::other("invalid runtime command executable path"))?;
        if program.contains('\0')
            || arguments.iter().any(|value| value.contains('\0'))
            || environment.iter().any(|(key, value)| {
                key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
            })
        {
            return Err(io::Error::other("invalid runtime command argument or environment").into());
        }
        let mut nonce = [0u8; 16];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| io::Error::other("cannot generate runtime command identity"))?;
        let id = CommandId(InstanceId(format!("command-{}", hex::encode(nonce))));
        let spec = OciSpec {
            root: OciRoot {
                path: "/".into(),
                readonly: false,
            },
            process: OciProcess {
                args: std::iter::once(program.to_owned())
                    .chain(arguments.iter().cloned())
                    .collect(),
                env: environment
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
                cwd: "/".into(),
                user: OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
            },
            mounts: Vec::new(),
            linux: OciLinux {
                namespaces: Vec::new(),
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
            port_mapping: None,
        };
        self.control.prepare(&id.0, &spec).await?;
        Ok(id)
    }

    /// Return every published attempt, refusing an incomplete or corrupt inventory.
    pub async fn inventory(&self) -> Result<Vec<CommandId>, CommandError> {
        Ok(self
            .control
            .inventory()
            .await?
            .into_iter()
            .map(|launch| CommandId(launch.instance_id))
            .collect())
    }

    /// Prune positively retired attempts and their logs under exclusive collection ownership.
    /// Prepared, running and uncertain attempts remain discoverable. Command IDs
    /// are never reused, so delayed starts cannot activate a replacement after pruning.
    /// Wait for any desired output before calling this operation.
    pub async fn prune_retired(&self) -> Result<usize, CommandError> {
        Ok(self.control.prune_retired_commands().await?)
    }

    /// Return this command's private log stem; append stdout or stderr extensions.
    pub fn log_stem(&self, id: &CommandId) -> Result<PathBuf, CommandError> {
        Ok(self.control.log_stem(&id.0)?)
    }

    /// Activate the prepared generation through its independent owner.
    pub async fn start(&self, id: &CommandId) -> Result<(), CommandError> {
        Ok(self.control.start(&id.0).await?)
    }

    /// Run an auxiliary command supervised by this command's independent owner.
    /// Cancelling the future closes its control socket and requests child retirement.
    /// Retirement of the main command waits for every accepted auxiliary command.
    pub async fn exec(&self, id: &CommandId, command: &[String]) -> Result<String, CommandError> {
        Ok(self.control.exec(&id.0, command).await?)
    }

    /// Observe positive owner evidence; missing control never means Retired.
    pub async fn state(&self, id: &CommandId) -> Result<CommandState, CommandError> {
        let record = self.control.status(&id.0).await?;
        match record.phase {
            OwnerPhase::Prepared => Ok(CommandState::Prepared),
            OwnerPhase::Cancelled => Ok(CommandState::Cancelled),
            OwnerPhase::Running { pid } => Ok(CommandState::Running { pid }),
            OwnerPhase::Retired { exit_code } => Ok(CommandState::Retired { exit_code }),
            // status normally finishes this phase under the exclusive owner lock.
            OwnerPhase::Retiring { .. } => {
                Err(io::Error::other("runtime command control retirement is incomplete").into())
            }
        }
    }

    async fn terminal(&self, id: &CommandId) -> Result<CommandState, CommandError> {
        loop {
            match self.state(id).await {
                Ok(state @ (CommandState::Cancelled | CommandState::Retired { .. })) => {
                    return Ok(state);
                }
                Ok(_) => {}
                Err(CommandError::Io(error)) if transient_control_error(&error) => {}
                Err(error) => return Err(error),
            }
            // Polling can race a closing socket or a busy owner. Only a later
            // positive terminal record can complete this bounded wait.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait for actual completion and bounded output. Timeout does not cancel
    /// the command or discharge its caller's cleanup obligation. Transient control
    /// failures retry within the same deadline; invalid ownership still refuses.
    pub async fn wait(
        &self,
        id: &CommandId,
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        tokio::time::timeout(timeout, async {
            let CommandState::Retired { exit_code } = self.terminal(id).await? else {
                return Err(CommandError::Cancelled);
            };
            let stem = self.control.log_stem(&id.0)?;
            tokio::task::spawn_blocking(move || {
                let mut remaining = OUTPUT_LIMIT;
                let stdout = read_output(&stem.with_extension("stdout"), &mut remaining)?;
                let stderr = read_output(&stem.with_extension("stderr"), &mut remaining)?;
                Ok(CommandOutput {
                    exit_code,
                    stdout,
                    stderr,
                })
            })
            .await
            .map_err(io::Error::other)?
        })
        .await
        .map_err(|_| CommandError::TimedOut {
            command: id.clone(),
            timeout,
        })?
    }

    /// Request cancellation and wait for confirmed retirement. Any error leaves
    /// the record available for recovery and must prevent resource retirement.
    pub async fn retire(&self, id: &CommandId, timeout: Duration) -> Result<(), CommandError> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.control.signal(&id.0, true).await {
                    Ok(()) => break,
                    Err(error) if transient_control_error(&error) => {}
                    Err(error) => return Err(error.into()),
                }
                // A lost response does not establish either acceptance or exit.
                // Force-kill is idempotent for this immutable command identity.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.terminal(id).await?;
            Ok(())
        })
        .await
        .map_err(|_| CommandError::TimedOut {
            command: id.clone(),
            timeout,
        })?
    }
}

fn transient_control_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

fn read_output(path: &Path, remaining: &mut u64) -> Result<Vec<u8>, CommandError> {
    let bytes = crate::durable::read_bounded(path, *remaining, crate::durable::Access::Regular)
        .map_err(|error| match error.kind() {
            io::ErrorKind::FileTooLarge => CommandError::OutputTooLarge,
            _ => error.into(),
        })?;
    *remaining -= bytes.len() as u64;
    Ok(bytes)
}

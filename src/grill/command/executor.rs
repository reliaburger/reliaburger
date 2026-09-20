//! Command execution shared by direct adapters and claimed OCI generations.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::CommandOutput;
use crate::grill::runc_intent::{IntentCommands, IntentPhase};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Executes a bounded short runtime command without shell interpolation.
pub trait RuntimeCommandExecutor: Send + Sync {
    /// Return actual exit evidence and captured output, or retain uncertainty as an error.
    fn output(
        &self,
        program: &str,
        arguments: &[&str],
    ) -> impl std::future::Future<Output = io::Result<CommandOutput>> + Send;
}

/// Direct CLI execution for standalone callers without durable runtime ownership.
#[derive(Debug, Clone, Copy)]
pub struct DirectCommandExecutor;

impl RuntimeCommandExecutor for DirectCommandExecutor {
    async fn output(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        let output = tokio::time::timeout(
            COMMAND_TIMEOUT,
            tokio::process::Command::new(program)
                .args(arguments)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "runtime command exceeded five seconds",
            )
        })??;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Shared handles to one exclusive generation's short-command collection.
///
/// Each worker retains the mutex and generation claim through completion. Clones
/// created before sealing cannot acquire cleanup authority or reopen admission.
/// An execution error consumes the live claim; recover its durable generation
/// explicitly before retrying cleanup. No error becomes proof of absence.
#[derive(Debug, Clone)]
pub struct ClaimedCommandExecutor {
    commands: Arc<Mutex<Option<IntentCommands>>>,
    cleanup: bool,
}

impl ClaimedCommandExecutor {
    /// Open normal command admission for an already claimed generation.
    pub fn new(commands: IntentCommands) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Some(commands))),
            cleanup: false,
        }
    }

    /// Fence normal handles and return a cleanup handle after positive command draining.
    pub async fn seal(&self, timeout: Duration) -> io::Result<Self> {
        let mut guard = self.commands.clone().lock_owned().await;
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let current = guard.take().ok_or_else(unavailable)?;
            *guard = Some(current.seal(timeout).await?);
            Ok(Self {
                commands,
                cleanup: true,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Release the generation only after the caller has confirmed resource absence.
    pub async fn finish(&self, exit_code: Option<i32>) -> io::Result<()> {
        if !self.cleanup {
            return Err(io::Error::other(
                "normal runtime handle cannot finish cleanup",
            ));
        }
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            guard
                .take()
                .ok_or_else(unavailable)?
                .finish(exit_code)
                .await
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl RuntimeCommandExecutor for ClaimedCommandExecutor {
    async fn output(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        let program = program.to_owned();
        let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
        let cleanup = self.cleanup;
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            let current = guard.as_ref().ok_or_else(unavailable)?;
            let admitted = current.record().is_some_and(|record| {
                if cleanup {
                    record.phase == IntentPhase::Retiring
                } else {
                    record.phase == IntentPhase::Owned
                }
            });
            // A stale clone's refusal must leave the cleanup handle intact.
            if !admitted {
                return Err(io::Error::other("runtime command admission is sealed"));
            }
            let current = guard.take().ok_or_else(unavailable)?;
            let (current, output) = if cleanup {
                current
                    .run_cleanup(
                        Path::new(&program),
                        &arguments,
                        &BTreeMap::new(),
                        COMMAND_TIMEOUT,
                    )
                    .await?
            } else {
                current
                    .run(
                        Path::new(&program),
                        &arguments,
                        &BTreeMap::new(),
                        COMMAND_TIMEOUT,
                    )
                    .await?
            };
            *guard = Some(current);
            Ok(output)
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn unavailable() -> io::Error {
    io::Error::other("runtime command claim is unavailable; recover the recorded generation")
}

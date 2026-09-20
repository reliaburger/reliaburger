//! Short runtime mutations admitted under an exclusive generation claim.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::grill::command::{CommandOutput, CommandState, OwnedCommands};

use super::{IntentClaim, IntentPhase, RuntimeIntent};

/// Command admission and retirement for one exclusively claimed OCI generation.
///
/// Each operation retains the claim until its worker finishes, even when its
/// caller cancels. Errors leave durable intent and command ownership for retry.
/// A recovered collection must seal and drain commands before resource cleanup.
#[derive(Debug)]
pub struct IntentCommands {
    claim: IntentClaim,
    commands: OwnedCommands,
    drained: bool,
}

impl IntentClaim {
    /// Attach short runtime mutations to this exact published generation.
    /// The Bun executable provides the independent foreground command owner.
    pub fn supervise_commands(self, executable: PathBuf) -> io::Result<IntentCommands> {
        let record = self
            .record
            .as_ref()
            .ok_or_else(|| io::Error::other("runtime commands require published intent"))?;
        let directory = self
            .journal
            .directory
            .join("records")
            .join(&self.instance.0)
            .join("generations")
            .join(&record.generation.0)
            .join("mutations");
        Ok(IntentCommands {
            claim: self,
            commands: OwnedCommands::new(directory, executable),
            drained: false,
        })
    }

    async fn begin_retirement(mut self) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let record = self
                .record
                .as_mut()
                .ok_or_else(|| io::Error::other("no runtime intent to seal"))?;
            if !matches!(record.phase, IntentPhase::Retired { .. }) {
                record.phase = IntentPhase::Retiring;
                super::persist(
                    &self
                        .journal
                        .directory
                        .join("records")
                        .join(&self.instance.0),
                    record,
                )?;
            }
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl IntentCommands {
    /// Original request and retirement phase protected by this collection.
    pub fn record(&self) -> Option<&RuntimeIntent> {
        self.claim.record()
    }

    /// Run a short mutation while retaining exclusive generation authority.
    /// Non-zero exit status is returned as output; the runtime interprets it.
    /// A wait timeout retains the command and prevents further normal admission.
    pub async fn run(
        self,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> io::Result<(Self, CommandOutput)> {
        self.execute(program, arguments, environment, timeout, false)
            .await
    }

    /// Run a cleanup mutation only after sealing and draining this generation.
    /// Every earlier cleanup command must also have positively retired.
    pub async fn run_cleanup(
        self,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> io::Result<(Self, CommandOutput)> {
        self.execute(program, arguments, environment, timeout, true)
            .await
    }

    async fn execute(
        self,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        timeout: Duration,
        cleanup: bool,
    ) -> io::Result<(Self, CommandOutput)> {
        let program = program.to_path_buf();
        let arguments = arguments.to_vec();
        let environment = environment.clone();
        // Dropping this JoinHandle detaches the worker. The claim remains in it
        // through publication and execution, including nested blocking workers.
        tokio::spawn(async move {
            let record = self
                .claim
                .record
                .as_ref()
                .ok_or_else(|| io::Error::other("runtime commands require published intent"))?;
            let admitted = if cleanup {
                self.drained && record.phase == IntentPhase::Retiring
            } else {
                record.phase == IntentPhase::Owned
            };
            if !admitted {
                return Err(io::Error::other(
                    "runtime command admission is sealed or cleanup is undrained",
                ));
            }
            self.require_terminal_commands().await?;
            let id = self
                .commands
                .prepare(&program, &arguments, &environment)
                .await
                .map_err(io::Error::other)?;
            self.commands.start(&id).await.map_err(io::Error::other)?;
            let output = self
                .commands
                .wait(&id, timeout)
                .await
                .map_err(io::Error::other)?;
            Ok((self, output))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Persist refusal of new work, then positively retire every admitted mutator.
    /// Caller cancellation retains the claim through this bounded drain.
    /// A failed or timed-out drain leaves the generation sealed for recovery.
    pub async fn seal(mut self, timeout: Duration) -> io::Result<Self> {
        tokio::spawn(async move {
            self.claim = self.claim.begin_retirement().await?;
            tokio::time::timeout(timeout, async {
                for id in self.commands.inventory().await.map_err(io::Error::other)? {
                    self.commands
                        .retire(&id, timeout)
                        .await
                        .map_err(io::Error::other)?;
                }
                self.require_terminal_commands().await
            })
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "runtime command retirement timed out",
                )
            })??;
            self.drained = true;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Confirm retirement after the runtime has separately verified resource absence.
    /// The command inventory is rechecked while the claim still excludes admissions.
    pub async fn finish(self, exit_code: Option<i32>) -> io::Result<()> {
        tokio::spawn(async move {
            if !self.drained {
                return Err(io::Error::other(
                    "runtime commands must be drained before retirement",
                ));
            }
            self.require_terminal_commands().await?;
            self.claim.retire(exit_code).await?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn require_terminal_commands(&self) -> io::Result<()> {
        for id in self.commands.inventory().await.map_err(io::Error::other)? {
            if !matches!(
                self.commands.state(&id).await.map_err(io::Error::other)?,
                CommandState::Cancelled | CommandState::Retired { .. }
            ) {
                return Err(io::Error::other(
                    "an earlier runtime command has not retired",
                ));
            }
        }
        Ok(())
    }
}

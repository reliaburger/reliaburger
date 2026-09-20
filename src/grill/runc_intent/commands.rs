//! Short runtime mutations admitted under an exclusive generation claim.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::grill::command::{CommandId, CommandOutput, CommandState, OwnedCommands};

use super::{IntentClaim, IntentPhase, RuntimeIntent};

/// Long-lived command whose identity must precede activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeRole {
    /// Foreground Runc launcher and its container descendants.
    Launcher,
    /// Rootless network helper for this container.
    RootlessNetwork,
}

impl RuntimeRole {
    const ALL: [Self; 2] = [Self::Launcher, Self::RootlessNetwork];
}

/// Command admission and retirement for one exclusively claimed OCI generation.
///
/// Each operation retains the claim until its worker finishes, even when its
/// caller cancels. Errors leave durable intent and command ownership for retry.
/// A recovered collection must seal and drain commands before resource cleanup.
#[derive(Debug)]
pub struct IntentCommands {
    claim: IntentClaim,
    commands: OwnedCommands,
    launcher: OwnedCommands,
    rootless_network: OwnedCommands,
    drained: bool,
}

/// A captured owner capability; it cannot register independent role commands.
pub(crate) struct RoleExecution {
    commands: OwnedCommands,
    id: CommandId,
}

impl RoleExecution {
    /// Wait through the captured owner; caller cancellation closes its exec socket.
    pub(crate) async fn execute(&self, command: &[String]) -> io::Result<String> {
        self.commands
            .exec(&self.id, command)
            .await
            .map_err(io::Error::other)
    }
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
            .join(&record.generation.0);
        Ok(IntentCommands {
            claim: self,
            commands: OwnedCommands::new(directory.join("mutations"), executable.clone()),
            launcher: OwnedCommands::new(directory.join("launcher"), executable.clone()),
            rootless_network: OwnedCommands::new(directory.join("rootless-network"), executable),
            drained: false,
        })
    }

    async fn bind_role(mut self, role: RuntimeRole, id: CommandId) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let record = self
                .record
                .as_mut()
                .ok_or_else(|| io::Error::other("no runtime intent to bind"))?;
            if record.phase != IntentPhase::Owned {
                return Err(io::Error::other("runtime role admission is sealed"));
            }
            match role {
                RuntimeRole::Launcher => {
                    if record.roles.launcher.is_some() {
                        return Err(io::Error::other("runtime launcher is already bound"));
                    }
                    record.roles.launcher = Some(id);
                }
                RuntimeRole::RootlessNetwork => {
                    if let Some(previous) = record.roles.rootless_network.replace(id) {
                        record.roles.retired_rootless_network.push(previous);
                    }
                }
            }
            super::persist(
                &self
                    .journal
                    .directory
                    .join("records")
                    .join(&self.instance.0),
                record,
            )?;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
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

    fn collection(&self, role: RuntimeRole) -> &OwnedCommands {
        match role {
            RuntimeRole::Launcher => &self.launcher,
            RuntimeRole::RootlessNetwork => &self.rootless_network,
        }
    }

    fn binding(&self, role: RuntimeRole) -> Option<&CommandId> {
        let record = self.record()?;
        match role {
            RuntimeRole::Launcher => record.roles.launcher.as_ref(),
            RuntimeRole::RootlessNetwork => record.roles.rootless_network.as_ref(),
        }
    }

    // Check every role before signalling any command. An unbound prepared
    // attempt is a crash before activation permission; executed ones are corruption.
    async fn validate_roles(&self) -> io::Result<()> {
        let record = self
            .record()
            .ok_or_else(|| io::Error::other("no runtime role intent"))?;
        for role in RuntimeRole::ALL {
            let collection = self.collection(role);
            let ids = collection.inventory().await.map_err(io::Error::other)?;
            let mut bindings: HashSet<&CommandId> = self.binding(role).into_iter().collect();
            let previous = match role {
                RuntimeRole::Launcher => &[][..],
                RuntimeRole::RootlessNetwork => record.roles.retired_rootless_network.as_slice(),
            };
            if !previous.is_empty() && self.binding(role).is_none() {
                return Err(io::Error::other(
                    "retired helpers have no current role binding",
                ));
            }
            for previous in previous {
                if !bindings.insert(previous) {
                    return Err(io::Error::other("duplicate runtime helper binding"));
                }
            }
            let observed: HashSet<&CommandId> = ids.iter().collect();
            if !bindings.is_subset(&observed) {
                return Err(io::Error::other(
                    "runtime role binding does not match command inventory",
                ));
            }
            for id in ids {
                let state = collection.state(&id).await.map_err(io::Error::other)?;
                if !bindings.contains(&id) {
                    if !matches!(state, CommandState::Prepared | CommandState::Cancelled) {
                        return Err(io::Error::other("unbound runtime role has executed"));
                    }
                } else if Some(&id) != self.binding(role)
                    && !matches!(
                        state,
                        CommandState::Cancelled | CommandState::Retired { .. }
                    )
                {
                    return Err(io::Error::other("earlier network helper has not retired"));
                }
            }
        }
        Ok(())
    }

    async fn prepare_role_admission(&self, role: RuntimeRole) -> io::Result<()> {
        let collection = self.collection(role);
        let ids = collection.inventory().await.map_err(io::Error::other)?;
        if role == RuntimeRole::Launcher {
            if self.binding(role).is_some() || !ids.is_empty() {
                return Err(io::Error::other("runtime launcher already owns an attempt"));
            }
            return Ok(());
        }
        for id in ids {
            match collection.state(&id).await.map_err(io::Error::other)? {
                CommandState::Prepared => {
                    collection
                        .retire(&id, Duration::from_secs(15))
                        .await
                        .map_err(io::Error::other)?;
                }
                CommandState::Cancelled | CommandState::Retired { .. } => {}
                CommandState::Running { .. } => {
                    return Err(io::Error::other("runtime network helper is still running"));
                }
            }
        }
        // A pending owner's activation could race cancellation. Its positive
        // terminal record, not the earlier Prepared snapshot, permits replacement.
        self.validate_roles().await?;
        for id in collection.inventory().await.map_err(io::Error::other)? {
            if !matches!(
                collection.state(&id).await.map_err(io::Error::other)?,
                CommandState::Cancelled | CommandState::Retired { .. }
            ) {
                return Err(io::Error::other("runtime network helper has not retired"));
            }
        }
        Ok(())
    }

    /// Bind a long-lived command durably before asking its owner to activate it.
    /// Cancellation retains the claim through binding and activation acknowledgement.
    /// Only a network helper may replace an earlier positively retired role;
    /// the workload launcher is never implicitly repeated.
    pub async fn start_role(
        mut self,
        role: RuntimeRole,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
    ) -> io::Result<Self> {
        let program = program.to_owned();
        let arguments = arguments.to_vec();
        let environment = environment.clone();
        tokio::spawn(async move {
            if !self
                .record()
                .is_some_and(|record| record.phase == IntentPhase::Owned)
            {
                return Err(io::Error::other("runtime role admission is sealed"));
            }
            self.validate_roles().await?;
            self.require_terminal_commands().await?;
            self.prepare_role_admission(role).await?;
            let id = self
                .collection(role)
                .prepare(&program, &arguments, &environment)
                .await
                .map_err(io::Error::other)?;
            self.claim = self.claim.bind_role(role, id.clone()).await?;
            self.collection(role)
                .start(&id)
                .await
                .map_err(io::Error::other)?;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Recover positive state from the exact role binding, never an adopted PID.
    /// None means no command was authorised for this role, including prepared attempts.
    pub async fn role_state(&self, role: RuntimeRole) -> io::Result<Option<CommandState>> {
        self.validate_roles().await?;
        match self.binding(role) {
            Some(id) => Ok(Some(
                self.collection(role)
                    .state(id)
                    .await
                    .map_err(io::Error::other)?,
            )),
            None => Ok(None),
        }
    }

    /// Capture the immutable owner capability before releasing the adapter mutex.
    /// The independent owner fences and retires auxiliary work, including requests
    /// queued before sealing but delivered while retirement is in progress.
    pub(crate) async fn execution(&self, role: RuntimeRole) -> io::Result<RoleExecution> {
        if !self
            .record()
            .is_some_and(|record| record.phase == IntentPhase::Owned)
        {
            return Err(io::Error::other("runtime exec admission is sealed"));
        }
        if !matches!(
            self.role_state(role).await?,
            Some(CommandState::Running { .. })
        ) {
            return Err(io::Error::other("runtime role is not running"));
        }
        let id = self
            .binding(role)
            .ok_or_else(|| io::Error::other("runtime role is unbound"))?
            .clone();
        Ok(RoleExecution {
            commands: self.collection(role).clone(),
            id,
        })
    }

    /// Locate the bound role's original logs after validating its complete inventory.
    pub async fn role_log_stem(&self, role: RuntimeRole) -> io::Result<Option<PathBuf>> {
        self.validate_roles().await?;
        self.binding(role)
            .map(|id| self.collection(role).log_stem(id).map_err(io::Error::other))
            .transpose()
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
            self.validate_roles().await?;
            self.require_terminal_commands().await?;
            self.commands
                .prune_retired()
                .await
                .map_err(io::Error::other)?;
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
                self.validate_roles().await?;
                for role in RuntimeRole::ALL {
                    let collection = self.collection(role);
                    for id in collection.inventory().await.map_err(io::Error::other)? {
                        collection
                            .retire(&id, timeout)
                            .await
                            .map_err(io::Error::other)?;
                    }
                }
                for id in self.commands.inventory().await.map_err(io::Error::other)? {
                    self.commands
                        .retire(&id, timeout)
                        .await
                        .map_err(io::Error::other)?;
                }
                self.require_terminal_roles().await?;
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

    /// Persist a discovery hold while retaining exclusive generation authority.
    pub async fn retain_network(mut self, container_index: u16) -> io::Result<Self> {
        self.claim = self.claim.retain_network(container_index).await?;
        Ok(self)
    }

    /// Persist the original publisher's release before address retirement.
    pub async fn release_network(mut self, reference: super::NetworkReference) -> io::Result<Self> {
        self.claim = self.claim.release_network(reference).await?;
        Ok(self)
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
            self.require_terminal_roles().await?;
            self.require_terminal_commands().await?;
            self.claim.retire(exit_code).await?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn require_terminal_roles(&self) -> io::Result<()> {
        self.validate_roles().await?;
        for role in RuntimeRole::ALL {
            let collection = self.collection(role);
            for id in collection.inventory().await.map_err(io::Error::other)? {
                if !matches!(
                    collection.state(&id).await.map_err(io::Error::other)?,
                    CommandState::Cancelled | CommandState::Retired { .. }
                ) {
                    return Err(io::Error::other("runtime role has not retired"));
                }
            }
        }
        Ok(())
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

//! Original OCI requests and exclusive authority over their runtime generations.
//!
//! Publish intent before allocating any resource. Keep the returned claim for
//! the entire lifecycle operation, including detached or blocking workers. A
//! caller may mark intent Retired only after every admitted command and resource
//! is positively absent. This journal does not establish that evidence itself.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::libc;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use super::{InstanceId, OciSpec};

mod commands;
pub use commands::IntentCommands;

const RECORD_LIMIT: u64 = 1024 * 1024;

/// Runtime configuration whose meaning must remain unchanged during recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentConfiguration {
    /// Absolute directory containing the instance's prepared bundle.
    pub bundle_directory: PathBuf,
    /// Absolute Runc state root passed to every CLI invocation.
    pub state_directory: PathBuf,
    /// Absolute image cache used to prepare the root filesystem.
    pub image_directory: PathBuf,
    /// Runc executable, either an explicit path or its PATH-resolved name.
    pub runc_program: PathBuf,
    /// Whether preparation uses rootless user namespaces and networking.
    pub rootless: bool,
    /// Node subnet index used by rootful address reservations.
    pub node_index: u16,
}

/// Identity of one immutable preparation attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntentGeneration(String);

/// Whether this generation still owns a cleanup obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntentPhase {
    /// Preparation or execution may have created resources; absence is unproven.
    Owned,
    /// New workload mutations are fenced while commands and resources retire.
    Retiring,
    /// The runtime confirmed every command and resource retired.
    Retired {
        /// Actual workload exit code, when known independently of cleanup.
        exit_code: Option<i32>,
    },
}

/// Immutable original request plus the runtime's latest retirement evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIntent {
    version: u32,
    /// Workload name shared with the agent's independent adoption record.
    pub instance_id: InstanceId,
    /// Generation required by any subsequent mutation.
    pub generation: IntentGeneration,
    /// Original specification, before image, rootfs or network preparation.
    pub spec: OciSpec,
    /// Runtime configuration that gave this request its meaning.
    pub configuration: IntentConfiguration,
    /// Whether replacement has been authorised by confirmed retirement.
    pub phase: IntentPhase,
}

/// A node's persistent collection of original Runc preparation attempts.
#[derive(Debug, Clone)]
pub struct IntentJournal {
    directory: PathBuf,
    configuration: IntentConfiguration,
}

// Never unlink or replace a lock file: another process may already hold it.
// Explicit unlock also prevents forked, pre-exec children extending its lifetime.
#[derive(Debug)]
struct LifecycleLock(File);

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Exclusive lifecycle authority shared across independently opened adapters.
///
/// Keep this claim alive through all effects it authorises. Persistence consumes
/// and returns it so cancelling an async caller cannot release the lock while a
/// blocking write continues. Dropping a claim does not retire its generation.
#[derive(Debug)]
pub struct IntentClaim {
    journal: IntentJournal,
    instance: InstanceId,
    record: Option<RuntimeIntent>,
    _lock: LifecycleLock,
}

impl IntentJournal {
    /// Open a journal lazily, without treating absent storage as launched work.
    pub fn new(directory: PathBuf, configuration: IntentConfiguration) -> Self {
        Self {
            directory,
            configuration,
        }
    }

    /// Read the current generation before queueing any operation that can mutate it.
    /// A published directory with missing or invalid intent always refuses.
    pub async fn observe(&self, instance: &InstanceId) -> io::Result<Option<IntentGeneration>> {
        let journal = self.clone();
        let instance = instance.clone();
        tokio::task::spawn_blocking(move || {
            Ok(journal.load(&instance)?.map(|record| record.generation))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Acquire authority only if the previously observed generation still matches.
    /// Busy owners refuse immediately; callers must not reinterpret this as absence.
    pub async fn claim(
        &self,
        instance: &InstanceId,
        expected: Option<IntentGeneration>,
    ) -> io::Result<IntentClaim> {
        let journal = self.clone();
        let instance = instance.clone();
        tokio::task::spawn_blocking(move || {
            validate_instance(&instance)?;
            journal.validate_configuration()?;
            create_directory(&journal.directory)?;
            create_directory(&journal.directory.join("records"))?;
            let locks = journal.directory.join("locks");
            create_directory(&locks)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(locks.join(&instance.0))?;
            validate_file(&file)?;
            file.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => {
                    io::Error::new(io::ErrorKind::WouldBlock, "runtime lifecycle is busy")
                }
                std::fs::TryLockError::Error(error) => error,
            })?;
            let lock = LifecycleLock(file);
            let record = journal.load(&instance)?;
            if record.as_ref().map(|record| &record.generation) != expected.as_ref() {
                return Err(io::Error::other("runtime intent generation changed"));
            }
            Ok(IntentClaim {
                journal,
                instance,
                record,
                _lock: lock,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Validate and return the entire published inventory, including retired work.
    /// The caller must exclude concurrent registration when using this as startup
    /// evidence; individual lifecycle locks do not freeze the whole collection.
    pub async fn inventory(&self) -> io::Result<Vec<RuntimeIntent>> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || {
            journal.validate_configuration()?;
            if !existing_directory(&journal.directory)? {
                return Ok(Vec::new());
            }
            let records = journal.directory.join("records");
            if !existing_directory(&records)? {
                return Ok(Vec::new());
            }
            let mut inventory = Vec::new();
            for entry in std::fs::read_dir(&records)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| io::Error::other("non-UTF-8 runtime intent identity"))?;
                if name.starts_with(".preparing-") && entry.file_type()?.is_dir() {
                    // No caller may allocate resources until atomic publication.
                    validate_directory(&entry.path())?;
                    continue;
                }
                inventory.push(journal.load(&InstanceId(name))?.ok_or_else(|| {
                    io::Error::other("runtime intent disappeared during inventory")
                })?);
            }
            inventory.sort_by(|left, right| left.instance_id.0.cmp(&right.instance_id.0));
            Ok(inventory)
        })
        .await
        .map_err(io::Error::other)?
    }

    fn validate_configuration(&self) -> io::Result<()> {
        let configuration = &self.configuration;
        if !self.directory.is_absolute()
            || !configuration.bundle_directory.is_absolute()
            || !configuration.state_directory.is_absolute()
            || !configuration.image_directory.is_absolute()
            || configuration.runc_program.as_os_str().is_empty()
            || configuration.node_index == 0
            || configuration.node_index > 32767
        {
            return Err(io::Error::other("invalid runtime intent configuration"));
        }
        Ok(())
    }

    fn load(&self, instance: &InstanceId) -> io::Result<Option<RuntimeIntent>> {
        validate_instance(instance)?;
        self.validate_configuration()?;
        if !existing_directory(&self.directory)? {
            return Ok(None);
        }
        let records = self.directory.join("records");
        if !existing_directory(&records)? {
            return Ok(None);
        }
        let directory = records.join(&instance.0);
        if !existing_directory(&directory)? {
            return Ok(None);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("intent.json"))?;
        validate_file(&file)?;
        let mut bytes = Vec::new();
        file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > RECORD_LIMIT {
            return Err(io::Error::other("runtime intent exceeds size limit"));
        }
        let record: RuntimeIntent = serde_json::from_slice(&bytes)?;
        if record.version != 1
            || record.instance_id != *instance
            || record.configuration != self.configuration
            || record.generation.0.len() != 32
            || !record
                .generation
                .0
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(io::Error::other("invalid or incompatible runtime intent"));
        }
        Ok(Some(record))
    }
}

impl IntentClaim {
    /// Original request protected by this claim, or none before first publication.
    pub fn record(&self) -> Option<&RuntimeIntent> {
        self.record.as_ref()
    }

    /// Publish a fresh generation before authorising its first resource mutation.
    /// Replacement requires the previous generation's confirmed retirement.
    pub async fn publish(mut self, spec: &OciSpec) -> io::Result<Self> {
        let spec = spec.clone();
        tokio::task::spawn_blocking(move || {
            if self
                .record
                .as_ref()
                .is_some_and(|record| !matches!(record.phase, IntentPhase::Retired { .. }))
            {
                return Err(io::Error::other("runtime intent still owns resources"));
            }
            let mut nonce = [0u8; 16];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| io::Error::other("cannot generate runtime intent identity"))?;
            let record = RuntimeIntent {
                version: 1,
                instance_id: self.instance.clone(),
                generation: IntentGeneration(hex::encode(nonce)),
                spec,
                configuration: self.journal.configuration.clone(),
                phase: IntentPhase::Owned,
            };
            let records = self.journal.directory.join("records");
            let directory = records.join(&self.instance.0);
            if self.record.is_none() {
                let staging = tempfile::Builder::new()
                    .prefix(".preparing-")
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir_in(&records)?;
                persist(staging.path(), &record)?;
                std::fs::rename(staging.path(), &directory)?;
                File::open(&records)?.sync_all()?;
            } else {
                persist(&directory, &record)?;
            }
            self.record = Some(record);
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Record externally established retirement while retaining lifecycle authority.
    /// This call must follow positive retirement of every admitted command and
    /// owned resource. A deadline, missing PID or missing socket is insufficient.
    pub async fn retire(mut self, exit_code: Option<i32>) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let record = self
                .record
                .as_mut()
                .ok_or_else(|| io::Error::other("no runtime intent to retire"))?;
            if let IntentPhase::Retired {
                exit_code: recorded,
            } = record.phase
            {
                if recorded != exit_code {
                    return Err(io::Error::other(
                        "runtime exit evidence changed after retirement",
                    ));
                }
                return Ok(self);
            }
            record.phase = IntentPhase::Retired { exit_code };
            persist(
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
}

fn persist(directory: &Path, record: &RuntimeIntent) -> io::Result<()> {
    let bytes = serde_json::to_vec(record)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(io::Error::other("runtime intent exceeds size limit"));
    }
    crate::sesame::identity::atomic_write_mode(&directory.join("intent.json"), &bytes, Some(0o600))
}

fn validate_instance(instance: &InstanceId) -> io::Result<()> {
    let name = &instance.0;
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(io::Error::other("invalid runtime intent identity"));
    }
    Ok(())
}

fn validate_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("invalid private runtime intent file"));
    }
    Ok(())
}

fn validate_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("invalid private runtime intent directory"));
    }
    Ok(())
}

fn existing_directory(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            validate_directory(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn create_directory(path: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            File::open(path)?.sync_all()?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_directory(path)
}

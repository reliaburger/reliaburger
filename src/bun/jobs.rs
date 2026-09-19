//! Durable node-local job attempts, including executions with unknown outcomes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Durable inventory, separate from per-runtime JSON adoption records.
pub(super) const CHECKPOINT_FILE: &str = "job-attempts.checkpoint";
/// Automatic retries after the initial execution.
pub(super) const MAX_RETRIES: u32 = 3;
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

/// Evidence for one attempt; absence of an exit code is never a failure code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum JobPhase {
    /// Intent committed before create/start; no exit has been observed.
    Launching,
    /// The runtime reported an actual exit status.
    Exited {
        /// Observed process exit code, including zero for success.
        code: i32,
    },
    /// The attempt may have run, but its outcome cannot be established.
    Unknown,
    /// Operator stop intent; retirement must still be confirmed.
    Stopping,
    /// Operator stop completed without authorising another automatic retry.
    Stopped,
}

/// Latest run and its consumed budget, retained until explicit retirement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedJob {
    /// Workload name within its namespace.
    pub name: String,
    /// Namespace owning this execution.
    pub namespace: String,
    /// Complete non-scheduled execution specification for recovery.
    pub spec: crate::config::job::JobSpec,
    /// Runtime responsible for retirement and adoption.
    pub runtime: crate::grill::records::RuntimeKind,
    /// Explicit run generation; retries retain this value.
    pub generation: u64,
    /// Retry budget consumed before starting the current attempt.
    pub restart_count: u32,
    /// Durable execution or operator-stop evidence.
    pub phase: JobPhase,
    /// Positive runtime absence observation, never inferred from a missing file.
    pub runtime_absent: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    jobs: Vec<RecordedJob>,
}

fn validate(jobs: &BTreeMap<String, RecordedJob>) -> std::io::Result<()> {
    for (id, job) in jobs {
        let mut config = crate::config::Config::default();
        config.job.insert(job.name.clone(), job.spec.clone());
        config.validate().map_err(std::io::Error::other)?;
        if job.spec.namespace.as_deref().unwrap_or("default") != job.namespace
            || job.spec.schedule.is_some()
            || job.generation == 0
            || job.restart_count > MAX_RETRIES
            || crate::grill::InstanceIdentity::new(&job.namespace, &job.name, 0)
                .instance_id()
                .0
                != *id
        {
            return Err(std::io::Error::other(
                "invalid job attempt identity or budget",
            ));
        }
    }
    Ok(())
}

/// Only a missing file means an empty inventory; malformed state refuses startup.
pub(super) fn load(directory: &Path) -> std::io::Result<BTreeMap<String, RecordedJob>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(directory.join(CHECKPOINT_FILE)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other("invalid job attempt checkpoint file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other("job attempt checkpoint is too large"));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
    if checkpoint.schema != 1 {
        return Err(std::io::Error::other(
            "unsupported job attempt checkpoint schema",
        ));
    }
    let mut jobs = BTreeMap::new();
    for job in checkpoint.jobs {
        let id = crate::grill::InstanceIdentity::new(&job.namespace, &job.name, 0)
            .instance_id()
            .0;
        if jobs.insert(id, job).is_some() {
            return Err(std::io::Error::other("duplicate job attempt identity"));
        }
    }
    validate(&jobs)?;
    Ok(jobs)
}

/// Replace the private checkpoint and sync the directory containing it.
pub(super) fn persist(
    directory: &Path,
    jobs: BTreeMap<String, RecordedJob>,
) -> std::io::Result<()> {
    validate(&jobs)?;
    let bytes = serde_json::to_vec(&Checkpoint {
        schema: 1,
        jobs: jobs.into_values().collect(),
    })?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other("job attempt checkpoint is too large"));
    }
    std::fs::create_dir_all(directory)?;
    crate::sesame::identity::atomic_write_mode(
        &directory.join(CHECKPOINT_FILE),
        &bytes,
        Some(0o600),
    )?;
    if let Some(parent) = directory.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// A rerun is node-local and must not replay unrelated declarative resources.
pub(crate) fn validate_rerun(config: &crate::config::Config) -> Result<(), &'static str> {
    if config.job.is_empty()
        || !config.app.is_empty()
        || !config.namespace.is_empty()
        || !config.permission.is_empty()
        || !config.build.is_empty()
        || config.job.values().any(|spec| spec.schedule.is_some())
    {
        return Err("explicit rerun requires a manifest containing only non-scheduled jobs");
    }
    Ok(())
}

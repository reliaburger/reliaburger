//! Durable registrations and firing claims for node-local cron jobs.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Kept outside the `.json` instance-record inventory.
pub(super) const CHECKPOINT_FILE: &str = "scheduled-jobs.checkpoint";
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

/// One acknowledged schedule and its latest claimed UTC minute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedSchedule {
    /// Workload name within its namespace.
    pub name: String,
    /// Namespace matched against the stored specification.
    pub namespace: String,
    /// Complete specification to use for future scheduled launches.
    pub spec: crate::config::job::JobSpec,
    /// Latest claimed epoch minute, including a launch interrupted by a crash.
    pub last_fired_minute: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    jobs: Vec<RecordedSchedule>,
}

/// Read the complete checkpoint. Only a missing file means no registrations.
pub(super) fn load(directory: &Path) -> std::io::Result<Vec<RecordedSchedule>> {
    let Some(checkpoint) = crate::durable::read_json_if_exists::<Checkpoint>(
        &directory.join(CHECKPOINT_FILE),
        MAX_CHECKPOINT_BYTES,
        crate::durable::Access::Regular,
    )?
    else {
        return Ok(Vec::new());
    };
    if checkpoint.schema != 1 {
        return Err(std::io::Error::other(
            "unsupported scheduled-job checkpoint schema",
        ));
    }
    Ok(checkpoint.jobs)
}

/// Replace the complete checkpoint privately and durably, on a blocking worker.
pub(super) fn persist(directory: &Path, mut jobs: Vec<RecordedSchedule>) -> std::io::Result<()> {
    jobs.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
    let bytes = serde_json::to_vec(&Checkpoint { schema: 1, jobs })?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other(
            "scheduled-job checkpoint is too large",
        ));
    }
    std::fs::create_dir_all(directory)?;
    crate::sesame::identity::atomic_write_mode(
        &directory.join(CHECKPOINT_FILE),
        &bytes,
        Some(0o600),
    )?;
    // The records directory itself may have been created by this write.
    if let Some(parent) = directory.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

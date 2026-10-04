//! Durable node-local job attempts, including executions with unknown outcomes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub(super) const CHECKPOINT_FILE: &str = "job-attempts.checkpoint";
pub(super) const MAX_RETRIES: u32 = 3;
const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
// Covers phase, exit evidence and integer-width changes of every active record.
const ACTIVE_TRANSITION_HEADROOM: usize = 256;
// Load shares the publisher lock: a timed-out off-loop publication cannot later
// overwrite a newly recovered inventory in the same process.
static CHECKPOINT_IO: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum JobPhase {
    /// Budget claimed before create; execution has not been authorised.
    Preparing,
    /// Runtime preparation completed and execution was authorised durably.
    Launching,
    /// The runtime reported an actual exit status, including zero for success.
    Exited { code: i32 },
    /// The attempt may have run, but its outcome cannot be established.
    Unknown,
    /// Operator stop intent; retirement must still be confirmed.
    Stopping,
    /// Operator stop completed without authorising another automatic retry.
    Stopped,
}

/// Trusted ownership, never accepted through a public JobSpec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchExecutionOwnership {
    pub batch_id: u64,
    pub logical_name: String,
    pub spec_digest: String,
    /// Current attempt's observed exit, cleared before an automatic retry.
    pub observed_exit_code: Option<i32>,
    #[serde(default)]
    pub observed_restart_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedJob {
    /// Runtime execution name within its namespace; batch logical labels are separate.
    pub name: String,
    /// Namespace owning this execution.
    pub namespace: String,
    /// Complete admitted non-scheduled specification for recovery.
    pub spec: crate::config::job::JobSpec,
    /// Runtime responsible for retirement and adoption.
    pub runtime: crate::grill::records::RuntimeKind,
    /// Explicit run generation; automatic retries retain this value.
    pub generation: u64,
    /// Retry budget consumed before starting the current attempt.
    pub restart_count: u32,
    /// Durable execution or operator-stop evidence.
    pub phase: JobPhase,
    /// Positive runtime absence observation, never inferred from a missing file.
    pub runtime_absent: bool,
    // Missing ownership would erase a durable replay fence. Ordinary records
    // must explicitly encode null; the custom deserializer requires presence.
    #[serde(deserialize_with = "deserialize_batch_execution")]
    pub batch_execution: Option<BatchExecutionOwnership>,
}

fn deserialize_batch_execution<'de, D>(
    deserializer: D,
) -> Result<Option<BatchExecutionOwnership>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<BatchExecutionOwnership>::deserialize(deserializer)
}

impl RecordedJob {
    /// Bind positive exit evidence to the current attempt during live observation
    /// and recovery. Unknown phases cannot reuse a previous attempt's exit.
    pub(super) fn observe_phase(&mut self, phase: JobPhase) {
        if let Some(owner) = &mut self.batch_execution {
            match phase {
                JobPhase::Exited { code } => {
                    owner.observed_exit_code = Some(code);
                    owner.observed_restart_count = Some(self.restart_count);
                }
                JobPhase::Unknown | JobPhase::Preparing | JobPhase::Launching => {
                    owner.observed_exit_code = None;
                    owner.observed_restart_count = None;
                }
                JobPhase::Stopping | JobPhase::Stopped => {}
            }
        }
        self.phase = phase;
    }
    pub(super) fn logical_name(&self) -> &str {
        self.batch_execution
            .as_ref()
            .map_or(&self.name, |owner| &owner.logical_name)
    }

    /// Durable current process exit; transient failed attempts remain pending.
    /// OCI objects can remain after exit, so compact retirement separately
    /// requires positive runtime/resource absence.
    pub(super) fn batch_terminal_exit(&self) -> Option<i32> {
        let owner = self.batch_execution.as_ref()?;
        if owner.observed_restart_count != Some(self.restart_count) {
            return None;
        }
        let code = owner.observed_exit_code?;
        match self.phase {
            JobPhase::Exited { code: phase_code } if phase_code == code => {}
            JobPhase::Stopped => {}
            _ => return None,
        }
        (code == 0 || self.restart_count >= MAX_RETRIES).then_some(code)
    }
}

/// A positively retired execution keeps its replay fence without its full spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetiredBatchExecution {
    pub name: String,
    pub namespace: String,
    pub generation: u64,
    pub restart_count: u32,
    pub batch_execution: BatchExecutionOwnership,
    pub runtime_absent: bool,
    pub phase: JobPhase,
}

impl RetiredBatchExecution {
    pub(super) fn terminal_exit(&self) -> Option<i32> {
        match self.phase {
            JobPhase::Exited { code } => Some(code),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct JobInventory {
    pub jobs: BTreeMap<String, RecordedJob>,
    pub retired: BTreeMap<String, RetiredBatchExecution>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    jobs: Vec<RecordedJob>,
    retired_batch_executions: Vec<RetiredBatchExecution>,
}

fn identity(namespace: &str, name: &str) -> String {
    crate::grill::InstanceIdentity::new(namespace, name, 0)
        .instance_id()
        .0
}

fn validate_owner(owner: &BatchExecutionOwnership, restart_count: u32) -> std::io::Result<()> {
    if owner.batch_id == 0
        || !crate::config::valid_workload_label(&owner.logical_name)
        || !crate::meat::batch_execution::valid_digest(&owner.spec_digest)
        || owner.observed_exit_code.is_some() != owner.observed_restart_count.is_some()
        || owner
            .observed_restart_count
            .is_some_and(|count| count != restart_count)
    {
        return Err(std::io::Error::other(
            "invalid batch execution ownership or attempt evidence",
        ));
    }
    Ok(())
}

/// Whether `previous` already proved `job`'s digest. The digest covers the
/// namespace, the logical label and the spec, so an unchanged triple needs no
/// second hash.
fn digest_verified(previous: Option<&RecordedJob>, job: &RecordedJob) -> bool {
    previous.is_some_and(|previous| {
        previous.namespace == job.namespace
            && previous.spec == job.spec
            && previous
                .batch_execution
                .as_ref()
                .map(|owner| (&owner.logical_name, &owner.spec_digest))
                == job
                    .batch_execution
                    .as_ref()
                    .map(|owner| (&owner.logical_name, &owner.spec_digest))
    })
}

/// Check every record. `verified` holds records that already passed this
/// check, so an unchanged spec skips its digest: hashing every admitted spec
/// on every commit made publication cost grow with the whole inventory.
fn validate(
    inventory: &JobInventory,
    verified: &BTreeMap<String, RecordedJob>,
) -> std::io::Result<()> {
    for (id, job) in &inventory.jobs {
        crate::config::validate_job(&job.name, &job.spec).map_err(std::io::Error::other)?;
        if job.spec.namespace.as_deref().unwrap_or("default") != job.namespace
            || job.spec.schedule.is_some()
            || job.generation == 0
            || job.restart_count > MAX_RETRIES
            || identity(&job.namespace, &job.name) != *id
            || inventory.retired.contains_key(id)
        {
            return Err(std::io::Error::other(
                "invalid or colliding job attempt identity or budget",
            ));
        }
        if let Some(owner) = &job.batch_execution {
            validate_owner(owner, job.restart_count)?;
            if let JobPhase::Exited { code } = job.phase
                && owner.observed_exit_code != Some(code)
            {
                return Err(std::io::Error::other(
                    "conflicting owned current exit evidence",
                ));
            }
            if !digest_verified(verified.get(id), job)
                && owner.spec_digest
                    != crate::meat::batch_execution::spec_digest(
                        &job.namespace,
                        &owner.logical_name,
                        &job.spec,
                    )?
            {
                return Err(std::io::Error::other(
                    "batch specification digest does not match its admitted spec",
                ));
            }
        }
    }
    for (id, proof) in &inventory.retired {
        validate_owner(&proof.batch_execution, proof.restart_count)?;
        if !crate::config::valid_workload_label(&proof.namespace)
            || !crate::config::valid_workload_label(&proof.name)
            || proof.generation == 0
            || proof.restart_count > MAX_RETRIES
            || identity(&proof.namespace, &proof.name) != *id
            || !proof.runtime_absent
            || !matches!(proof.phase, JobPhase::Unknown | JobPhase::Exited { .. })
        {
            return Err(std::io::Error::other(
                "invalid retired batch execution proof",
            ));
        }
        if let JobPhase::Exited { code } = proof.phase
            && (proof.batch_execution.observed_exit_code != Some(code)
                || proof.batch_execution.observed_restart_count != Some(proof.restart_count)
                || (code != 0 && proof.restart_count < MAX_RETRIES))
        {
            return Err(std::io::Error::other(
                "retired outcome is not positively terminal",
            ));
        }
    }
    Ok(())
}

/// A validated job inventory with the checkpoint bytes it encodes to, so
/// admission publishes the bytes its preflight already produced.
pub(super) struct EncodedInventory {
    inventory: JobInventory,
    bytes: Vec<u8>,
}

impl EncodedInventory {
    /// The inventory and its checkpoint bytes.
    pub(super) fn into_parts(self) -> (JobInventory, Vec<u8>) {
        (self.inventory, self.bytes)
    }
}

/// Validate and serialise `inventory`. Each call costs time in proportion to
/// the whole inventory, so a commit makes exactly one.
pub(super) fn encode(
    inventory: &JobInventory,
    verified: &BTreeMap<String, RecordedJob>,
) -> std::io::Result<Vec<u8>> {
    validate(inventory, verified)?;
    Ok(serde_json::to_vec(&Checkpoint {
        schema: 3,
        jobs: inventory.jobs.values().cloned().collect(),
        retired_batch_executions: inventory.retired.values().cloned().collect(),
    })?)
}

/// Predictable refusal before the uncertain-I/O fence, with active-phase headroom.
pub(super) fn preflight(
    inventory: JobInventory,
    verified: &BTreeMap<String, RecordedJob>,
) -> std::io::Result<EncodedInventory> {
    let bytes = encode(&inventory, verified)?;
    let headroom = inventory
        .jobs
        .len()
        .checked_mul(ACTIVE_TRANSITION_HEADROOM)
        .and_then(|reserve| bytes.len().checked_add(reserve))
        .ok_or_else(|| std::io::Error::other("job inventory size overflow"))?;
    if headroom > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other(
            "job attempt inventory is full; retained replay proofs cannot be pruned",
        ));
    }
    Ok(EncodedInventory { inventory, bytes })
}

pub(super) fn load_inventory(directory: &Path) -> std::io::Result<JobInventory> {
    let _io = CHECKPOINT_IO
        .lock()
        .map_err(|_| std::io::Error::other("job checkpoint publisher lock poisoned"))?;
    let Some(checkpoint) = crate::durable::read_json_if_exists::<Checkpoint>(
        &directory.join(CHECKPOINT_FILE),
        MAX_CHECKPOINT_BYTES as u64,
        crate::durable::Access::Regular,
    )?
    else {
        return Ok(JobInventory::default());
    };
    if checkpoint.schema != 3 {
        return Err(std::io::Error::other(
            "unsupported job attempt checkpoint schema",
        ));
    }
    let mut inventory = JobInventory::default();
    for job in checkpoint.jobs {
        if inventory
            .jobs
            .insert(identity(&job.namespace, &job.name), job)
            .is_some()
        {
            return Err(std::io::Error::other("duplicate job attempt identity"));
        }
    }
    for proof in checkpoint.retired_batch_executions {
        if inventory
            .retired
            .insert(identity(&proof.namespace, &proof.name), proof)
            .is_some()
        {
            return Err(std::io::Error::other(
                "duplicate retired batch execution identity",
            ));
        }
    }
    validate(&inventory, &BTreeMap::new())?;
    Ok(inventory)
}

/// Durably replace the checkpoint with `bytes` from [`encode`].
pub(super) fn publish(directory: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other("job attempt checkpoint is too large"));
    }
    let _io = CHECKPOINT_IO
        .lock()
        .map_err(|_| std::io::Error::other("job checkpoint publisher lock poisoned"))?;
    std::fs::create_dir_all(directory)?;
    crate::sesame::identity::atomic_write_mode(
        &directory.join(CHECKPOINT_FILE),
        bytes,
        Some(0o600),
    )?;
    if let Some(parent) = directory.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn persist_inventory(directory: &Path, inventory: JobInventory) -> std::io::Result<()> {
    publish(directory, &encode(&inventory, &BTreeMap::new())?)
}

#[cfg(test)]
pub(super) fn load(directory: &Path) -> std::io::Result<BTreeMap<String, RecordedJob>> {
    load_inventory(directory).map(|inventory| inventory.jobs)
}

#[cfg(test)]
pub(super) fn persist(
    directory: &Path,
    jobs: BTreeMap<String, RecordedJob>,
) -> std::io::Result<()> {
    persist_inventory(
        directory,
        JobInventory {
            jobs,
            retired: BTreeMap::new(),
        },
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn labelled_job(label: &str) -> RecordedJob {
        let spec = crate::config::Config::parse("[job.batch-111]\nimage='proc-grill:image-ignored'\ncommand=['true']\nnamespace='team'\n")
            .unwrap().job.remove("batch-111").unwrap();
        use sha2::{Digest, Sha256};
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&("team", label, &spec)).unwrap())
        );
        serde_json::from_value(serde_json::json!({
            "name": "batch-111", "namespace": "team",
            "batch_execution": {
                "batch_id": 99,
                "logical_name": label,
                "spec_digest": digest,
                "observed_exit_code": null
            },
            "spec": spec, "runtime": "Process", "generation": 1,
            "restart_count": 0, "phase": "Unknown", "runtime_absent": false,
        }))
        .unwrap()
    }

    #[test]
    fn recovery_refuses_omitted_batch_ownership_in_a_schema_three_record() {
        let job = labelled_job("migration");
        let directory = tempfile::tempdir().unwrap();
        persist(
            directory.path(),
            BTreeMap::from([(identity(&job.namespace, &job.name), job)]),
        )
        .unwrap();
        let path = directory.path().join(CHECKPOINT_FILE);
        let mut checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(checkpoint["schema"], 3);
        checkpoint["jobs"][0]
            .as_object_mut()
            .unwrap()
            .remove("batch_execution");
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        assert!(
            load_inventory(directory.path()).is_err(),
            "omitting ownership must not recover a batch execution as an ordinary job"
        );
    }

    #[test]
    fn ordinary_schema_three_records_recover_with_explicit_null_ownership() {
        let mut job = labelled_job("migration");
        job.batch_execution = None;
        let id = identity(&job.namespace, &job.name);
        let directory = tempfile::tempdir().unwrap();
        persist(
            directory.path(),
            BTreeMap::from([(id.clone(), job.clone())]),
        )
        .unwrap();
        let checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join(CHECKPOINT_FILE)).unwrap())
                .unwrap();
        assert!(checkpoint["jobs"][0].get("batch_execution").is_some());
        assert!(checkpoint["jobs"][0]["batch_execution"].is_null());
        assert_eq!(load_inventory(directory.path()).unwrap().jobs[&id], job);
    }

    #[test]
    fn a_confirmed_process_exit_completes_an_owned_execution() {
        let mut job = labelled_job("migration");
        job.runtime_absent = true;
        job.observe_phase(JobPhase::Exited { code: 0 });
        assert_eq!(job.batch_terminal_exit(), Some(0));
    }

    #[test]
    fn a_confirmed_oci_exit_completes_without_claiming_object_retirement() {
        for runtime in [
            crate::grill::records::RuntimeKind::Runc,
            crate::grill::records::RuntimeKind::Apple,
        ] {
            let mut job = labelled_job("migration");
            job.runtime = runtime;
            job.runtime_absent = false;
            job.observe_phase(JobPhase::Exited { code: 0 });
            assert_eq!(
                job.batch_terminal_exit(),
                Some(0),
                "a retained OCI object does not imply a running process"
            );
            assert!(!job.runtime_absent);
            let proof = RetiredBatchExecution {
                name: job.name.clone(),
                namespace: job.namespace.clone(),
                generation: job.generation,
                restart_count: job.restart_count,
                batch_execution: job.batch_execution.unwrap(),
                runtime_absent: false,
                phase: JobPhase::Exited { code: 0 },
            };
            let inventory = JobInventory {
                jobs: BTreeMap::new(),
                retired: BTreeMap::from([(identity(&proof.namespace, &proof.name), proof)]),
            };
            let directory = tempfile::tempdir().unwrap();
            assert!(
                persist_inventory(directory.path(), inventory).is_err(),
                "completion must not manufacture absent object provenance"
            );
            assert!(!directory.path().join(CHECKPOINT_FILE).exists());
        }
    }

    #[test]
    fn checkpoint_recovery_preserves_a_batchs_logical_label_and_execution_identity() {
        let directory = tempfile::tempdir().unwrap();
        let id = crate::grill::InstanceIdentity::new("team", "batch-111", 0)
            .instance_id()
            .0;
        persist(
            directory.path(),
            BTreeMap::from([(id.clone(), labelled_job("migration"))]),
        )
        .unwrap();
        let loaded = load(directory.path()).unwrap();
        assert_eq!(loaded[&id].name, "batch-111");
        assert_eq!(loaded[&id].namespace, "team");
        let value = serde_json::to_value(&loaded[&id]).unwrap();
        assert_eq!(value["batch_execution"]["logical_name"], "migration");
    }

    #[test]
    fn invalid_logical_labels_cannot_enter_durable_job_state() {
        let directory = tempfile::tempdir().unwrap();
        let id = crate::grill::InstanceIdentity::new("team", "batch-111", 0)
            .instance_id()
            .0;
        for label in ["../migration", "", "Migration", "team/migration"] {
            assert!(
                persist(
                    directory.path(),
                    BTreeMap::from([(id.clone(), labelled_job(label))])
                )
                .is_err()
            );
            assert!(!directory.path().join(CHECKPOINT_FILE).exists());
        }
    }
    #[test]
    fn mismatched_owned_exit_evidence_cannot_be_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = labelled_job("migration");
        job.observe_phase(JobPhase::Exited { code: 0 });
        job.phase = JobPhase::Exited { code: 1 };
        let inventory = JobInventory {
            jobs: BTreeMap::from([(identity(&job.namespace, &job.name), job)]),
            retired: BTreeMap::new(),
        };
        assert!(
            persist_inventory(directory.path(), inventory).is_err(),
            "conflicting current exit evidence was published"
        );
    }

    #[test]
    fn mismatched_owned_exit_evidence_cannot_be_recovered() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = labelled_job("migration");
        job.observe_phase(JobPhase::Exited { code: 0 });
        persist_inventory(
            directory.path(),
            JobInventory {
                jobs: BTreeMap::from([(identity(&job.namespace, &job.name), job)]),
                retired: BTreeMap::new(),
            },
        )
        .unwrap();
        let file = directory.path().join(CHECKPOINT_FILE);
        let mut checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        checkpoint["jobs"][0]["phase"]["Exited"]["code"] = serde_json::json!(1);
        std::fs::write(file, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        assert!(
            load_inventory(directory.path()).is_err(),
            "conflicting current exit evidence was recovered"
        );
    }

    #[test]
    fn mismatched_owned_exit_evidence_cannot_complete_a_batch() {
        let mut job = labelled_job("migration");
        job.observe_phase(JobPhase::Exited { code: 0 });
        job.phase = JobPhase::Exited { code: 1 };
        assert_eq!(
            job.batch_terminal_exit(),
            None,
            "an inconsistent phase fabricated terminal success"
        );
    }
    fn prerequisite_attempt() -> RecordedJob {
        let spec = crate::config::Config::parse(
            "[job.migration]\nimage='proc-grill:image-ignored'\ncommand=['true']\nrun_before=['app.web']\n",
        )
        .unwrap()
        .job
        .remove("migration")
        .unwrap();
        serde_json::from_value(serde_json::json!({
            "name": "migration", "namespace": "default", "spec": spec,
            "runtime": "Process", "generation": 1, "restart_count": 0,
            "phase": {"Exited": {"code": 0}}, "runtime_absent": true, "batch_execution": null,
        }))
        .unwrap()
    }

    #[test]
    fn an_admitted_prerequisite_persists_and_recovers_without_its_apply_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let identity = crate::grill::InstanceIdentity::new("default", "migration", 0)
            .instance_id()
            .0;
        let attempt = prerequisite_attempt();
        persist(
            directory.path(),
            BTreeMap::from([(identity.clone(), attempt.clone())]),
        )
        .unwrap();
        assert_eq!(load(directory.path()).unwrap()[&identity], attempt);
    }

    #[test]
    fn intrinsic_job_validation_still_refuses_malformed_prerequisite_records() {
        let directory = tempfile::tempdir().unwrap();
        let identity = crate::grill::InstanceIdentity::new("default", "migration", 0)
            .instance_id()
            .0;
        let mut attempt = prerequisite_attempt();
        attempt.spec.exec = Some("/bin/true".into());
        assert!(persist(directory.path(), BTreeMap::from([(identity, attempt)])).is_err());
        assert!(!directory.path().join(CHECKPOINT_FILE).exists());
    }

    fn inventory_of(job: RecordedJob) -> JobInventory {
        JobInventory {
            jobs: BTreeMap::from([(identity(&job.namespace, &job.name), job)]),
            retired: BTreeMap::new(),
        }
    }

    #[test]
    fn a_record_already_verified_is_not_hashed_again() {
        // A digest that cannot match proves which records encoding rehashes.
        let mut job = labelled_job("migration");
        job.batch_execution.as_mut().unwrap().spec_digest = "0".repeat(64);
        assert!(
            encode(&inventory_of(job.clone()), &BTreeMap::new()).is_err(),
            "an unverified record must have its digest checked"
        );
        let verified = inventory_of(job.clone()).jobs;
        job.observe_phase(JobPhase::Launching);
        assert!(
            encode(&inventory_of(job), &verified).is_ok(),
            "a phase change rehashed a spec that was already verified"
        );
    }

    #[test]
    fn a_changed_spec_or_label_is_verified_again_under_a_verified_identity() {
        let job = labelled_job("migration");
        let verified = inventory_of(job.clone()).jobs;
        let mut respecified = job.clone();
        respecified.spec.command = Some(vec!["false".into()]);
        assert!(encode(&inventory_of(respecified), &verified).is_err());
        let mut relabelled = job;
        relabelled.batch_execution.as_mut().unwrap().logical_name = "other".into();
        assert!(encode(&inventory_of(relabelled), &verified).is_err());
    }

    #[test]
    fn an_encoded_inventory_publishes_without_encoding_again() {
        let job = labelled_job("migration");
        let (_, bytes) = preflight(inventory_of(job.clone()), &BTreeMap::new())
            .unwrap()
            .into_parts();
        let directory = tempfile::tempdir().unwrap();
        publish(directory.path(), &bytes).unwrap();
        assert_eq!(
            load(directory.path()).unwrap()[&identity(&job.namespace, &job.name)],
            job
        );
    }

    #[test]
    fn preflight_refuses_an_inventory_without_room_for_its_transitions() {
        let mut job = labelled_job("migration");
        job.spec.image = Some(format!("proc-grill:{}", "x".repeat(MAX_CHECKPOINT_BYTES)));
        let digest =
            crate::meat::batch_execution::spec_digest(&job.namespace, "migration", &job.spec)
                .unwrap();
        job.batch_execution.as_mut().unwrap().spec_digest = digest;
        assert!(preflight(inventory_of(job), &BTreeMap::new()).is_err());
    }
}

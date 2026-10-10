//! Durable definitions and trigger identities shared by singleton and array runs.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::task_array::{TaskArraySpec, validate_template};
use crate::config::job::JobSpec;

/// Bound reusable definitions independently of how many task indices they describe.
pub const MAX_JOB_DEFINITIONS: usize = 256;
/// Active and retained run provenance must stay bounded alongside result retention.
pub const MAX_JOB_RUNS: usize = 512;

fn singleton() -> TaskArraySpec {
    TaskArraySpec::with_count(1)
}

/// Reusable work and the policy used when a trigger creates a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDefinition {
    /// Complete execution template; trigger fields live outside the template.
    pub template: JobSpec,
    /// Indexed tasks; an omitted policy describes one task.
    #[serde(default = "singleton")]
    pub tasks: TaskArraySpec,
    /// Durable UTC schedule, if this definition runs periodically.
    #[serde(default)]
    pub cron: Option<CronPolicy>,
    /// Permit automatic replay after loss of an owner with an unknown outcome.
    #[serde(default)]
    pub replay_unknown: bool,
}

impl JobDefinition {
    /// Normalise a TOML job into one indexed task, retaining its execution contract.
    ///
    /// The job's own policy fields move out of the template into the
    /// definition. Omitted ones keep the defaults: four attempts and no
    /// deadline, or one attempt and 600 seconds for a `run_before` hook.
    pub fn from_spec(mut template: JobSpec) -> Self {
        let overlap = template.overlap.take().unwrap_or_default();
        let cron = template.schedule.take().map(|expression| CronPolicy {
            expression,
            overlap,
            missed: MissedRunPolicy::Skip,
        });
        let hook = !template.run_before.is_empty();
        template.run_before.clear();
        let mut tasks = TaskArraySpec::with_count(1);
        tasks.max_attempts = template
            .max_attempts
            .take()
            .unwrap_or(if hook { 1 } else { 4 });
        tasks.task_timeout_secs =
            template
                .task_timeout_secs
                .take()
                .unwrap_or(if hook { 600 } else { 0 });
        let replay_unknown = std::mem::take(&mut template.replay_unknown);
        Self {
            template,
            tasks,
            cron,
            replay_unknown,
        }
    }

    /// Validate the immutable definition before recording any revision or run.
    pub fn validate(&self) -> Result<(), String> {
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > 20 * 1024) {
            return Err("job definition exceeds 20 KiB".into());
        }
        self.tasks.validate().map_err(|e| e.to_string())?;
        validate_template(&self.template).map_err(|e| e.to_string())?;
        if let Some(cron) = &self.cron {
            super::cron::CronSchedule::parse(&cron.expression).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn digest(&self) -> Result<String, String> {
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }
}

/// TOML jobs name the overlap policy too, so it lives with the job config.
pub use crate::config::job::OverlapPolicy;

/// What a leader does with schedule minutes missed while it was unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedRunPolicy {
    /// Only the currently observed matching minute may launch; no backlog replay.
    #[default]
    Skip,
}

/// Explicit UTC scheduling policies, separate from a task's runtime template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronPolicy {
    /// Five-field UTC cron expression.
    pub expression: String,
    /// Treatment of overlapping occurrences.
    #[serde(default)]
    pub overlap: OverlapPolicy,
    /// Treatment of missed occurrences.
    #[serde(default)]
    pub missed: MissedRunPolicy,
}

/// Stable source identity of one requested run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTrigger {
    /// The same request identity cannot create two manual runs.
    Manual { request_id: String },
    /// A deployment operation may retry its same hook without launching it twice.
    Hook { operation_id: String },
    /// An occurrence belongs to one definition revision and UTC minute.
    Cron { minute: i64 },
}

impl RunTrigger {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Manual { request_id: id } | Self::Hook { operation_id: id }
                if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) =>
            {
                Err(
                    "run trigger identity must contain 1–128 bytes without control characters"
                        .into(),
                )
            }
            Self::Cron { minute } if *minute < 0 => {
                Err("cron occurrence precedes the epoch".into())
            }
            _ => Ok(()),
        }
    }
}

/// Admitted definition revision and its cursor, retained independently of results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionRecord {
    /// Current validated work and trigger policy.
    pub definition: JobDefinition,
    /// Monotonically increasing content revision.
    pub revision: u64,
    /// Latest processed UTC minute, including a deliberately skipped overlap.
    pub last_observed_minute: Option<i64>,
}

/// Immutable provenance of an admitted run; its execution snapshot lives in the array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    /// Logical workload name.
    pub name: String,
    /// Authorised workload namespace.
    pub namespace: String,
    /// Definition content revision at admission.
    pub revision: u64,
    /// Trigger deduplicated at admission.
    pub trigger: RunTrigger,
    /// Hash of the complete accepted definition, including policies.
    pub definition_digest: String,
    /// Captured unknown-outcome policy; definition updates cannot change it.
    pub replay_unknown: bool,
    /// Owners whose execution outcome needs positive evidence or acknowledged replay.
    pub unknown_owners: BTreeSet<crate::meat::NodeId>,
}

/// Deterministic job-definition and run transaction carried by the array store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobWrite {
    /// Atomically update a reusable definition and optionally create a manual/hook run.
    Put {
        name: String,
        namespace: String,
        definition: Box<JobDefinition>,
        trigger: Option<RunTrigger>,
        now_epoch_secs: u64,
    },
    /// Atomically claim a matching UTC occurrence and create or skip its run.
    Fire {
        name: String,
        namespace: String,
        revision: u64,
        minute: i64,
        now_epoch_secs: u64,
    },
}

/// Bounded replicated definition and run metadata accompanying the execution store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "CatalogWire", into = "CatalogWire")]
pub struct JobCatalog {
    definitions: BTreeMap<String, DefinitionRecord>,
    runs: BTreeMap<u64, RunRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogWire {
    definitions: BTreeMap<String, DefinitionRecord>,
    runs: BTreeMap<u64, RunRecord>,
}
impl From<JobCatalog> for CatalogWire {
    fn from(catalog: JobCatalog) -> Self {
        Self {
            definitions: catalog.definitions,
            runs: catalog.runs,
        }
    }
}
impl TryFrom<CatalogWire> for JobCatalog {
    type Error = String;
    fn try_from(wire: CatalogWire) -> Result<Self, Self::Error> {
        if wire.definitions.len() > MAX_JOB_DEFINITIONS || wire.runs.len() > MAX_JOB_RUNS {
            return Err("job catalogue exceeds its definition or provenance bound".into());
        }
        for (key, record) in &wire.definitions {
            let (namespace, name) = key.split_once('/').ok_or("invalid job definition key")?;
            validate_key(namespace, name)?;
            record.definition.validate()?;
            if record.revision == 0
                || record.definition.template.namespace.as_deref() != Some(namespace)
                || record.last_observed_minute.is_some_and(|minute| minute < 0)
            {
                return Err("invalid job definition revision, namespace or cursor".into());
            }
        }
        let mut triggers = BTreeSet::new();
        for (id, run) in &wire.runs {
            validate_key(&run.namespace, &run.name)?;
            run.trigger.validate()?;
            if *id == 0
                || run.unknown_owners.len() > 64
                || run.revision == 0
                || run.definition_digest.len() != 64
                || !run.definition_digest.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("invalid job run provenance".into());
            }
            let identity = serde_json::to_string(&(&run.namespace, &run.name, &run.trigger))
                .map_err(|e| e.to_string())?;
            if !triggers.insert(identity) {
                return Err("duplicate job run trigger".into());
            }
        }
        Ok(Self {
            definitions: wire.definitions,
            runs: wire.runs,
        })
    }
}

pub(crate) enum JobPlan {
    Register {
        definition: Box<JobDefinition>,
        run: RunRecord,
        now_epoch_secs: u64,
    },
    Existing(u64),
    Recorded,
}

fn validate_key(namespace: &str, name: &str) -> Result<(), String> {
    if !crate::config::valid_workload_label(namespace) || !crate::config::valid_workload_label(name)
    {
        return Err("job name and namespace must be DNS labels".into());
    }
    Ok(())
}

impl JobCatalog {
    pub(crate) fn stop(&mut self, namespace: &str, name: &str, forget: bool) -> Result<(), String> {
        let key = format!("{namespace}/{name}");
        let record = self
            .definitions
            .get_mut(&key)
            .ok_or("unknown job definition")?;
        if record.definition.cron.is_some() {
            record.revision = record
                .revision
                .checked_add(1)
                .ok_or("job definition revision exhausted")?;
            record.definition.cron = None;
        }
        if forget {
            self.definitions.remove(&key);
        }
        Ok(())
    }

    /// Bounded retained provenance; never enumerates task indices.
    pub fn runs(&self) -> impl Iterator<Item = (u64, &RunRecord)> {
        self.runs.iter().map(|(id, run)| (*id, run))
    }

    /// A reusable definition, including the durable cron cursor.
    pub fn definition(&self, namespace: &str, name: &str) -> Option<&DefinitionRecord> {
        self.definitions.get(&format!("{namespace}/{name}"))
    }
    /// Immutable admitted provenance for a retained run.
    pub fn run(&self, id: u64) -> Option<&RunRecord> {
        self.runs.get(&id)
    }
    /// Reusable definitions, bounded independently of task counts.
    pub fn definitions(&self) -> impl Iterator<Item = (&str, &DefinitionRecord)> {
        self.definitions
            .iter()
            .map(|(key, record)| (key.as_str(), record))
    }
    pub(crate) fn run_mut(&mut self, id: u64) -> Option<&mut RunRecord> {
        self.runs.get_mut(&id)
    }

    /// Bound run provenance by the same retained execution identities.
    pub(crate) fn retain_runs(&mut self, retained: &BTreeSet<u64>) {
        self.runs.retain(|id, _| retained.contains(id));
    }
    pub(crate) fn record_run(&mut self, id: u64, run: RunRecord) {
        self.runs.insert(id, run);
    }
    /// Prepare on a candidate copy; the caller publishes only after all execution checks pass.
    pub(crate) fn prepare(
        &mut self,
        write: &JobWrite,
        active: &BTreeSet<u64>,
    ) -> Result<JobPlan, String> {
        let (name, namespace) = match write {
            JobWrite::Put {
                name, namespace, ..
            }
            | JobWrite::Fire {
                name, namespace, ..
            } => (name, namespace),
        };
        validate_key(namespace, name)?;
        let key = format!("{namespace}/{name}");
        let (definition, revision, trigger, now_epoch_secs) = match write {
            JobWrite::Put {
                definition,
                trigger,
                now_epoch_secs,
                ..
            } => {
                let mut definition = (**definition).clone();
                if definition
                    .template
                    .namespace
                    .as_deref()
                    .is_some_and(|ns| ns != namespace)
                {
                    return Err("definition namespace disagrees with its job key".into());
                }
                definition.template.namespace = Some(namespace.clone());
                definition.validate()?;
                if let Some(trigger) = trigger {
                    trigger.validate()?;
                    if matches!(trigger, RunTrigger::Cron { .. }) {
                        return Err(
                            "cron occurrences must use the fenced schedule transaction".into()
                        );
                    }
                    if let Some((id, run)) = self.runs.iter().find(|(_, r)| {
                        r.name == *name && r.namespace == *namespace && r.trigger == *trigger
                    }) {
                        if run.definition_digest != definition.digest()? {
                            return Err(
                                "run trigger already names a different accepted definition".into(),
                            );
                        }
                        return Ok(JobPlan::Existing(*id));
                    }
                }
                if trigger.is_some()
                    && self.runs.iter().any(|(id, run)| {
                        active.contains(id)
                            && run.name == *name
                            && run.namespace == *namespace
                            && !run.unknown_owners.is_empty()
                    })
                {
                    return Err("an unknown run still owns this job; acknowledge replay before another admission".into());
                }
                let prior = self.definitions.get(&key);
                let revision = match prior {
                    Some(record) if record.definition == definition => record.revision,
                    Some(record) => record
                        .revision
                        .checked_add(1)
                        .ok_or("job definition revision exhausted")?,
                    None => {
                        if self.definitions.len() >= MAX_JOB_DEFINITIONS {
                            return Err("job definition limit reached".into());
                        }
                        self.runs
                            .values()
                            .filter(|run| run.name == *name && run.namespace == *namespace)
                            .map(|run| run.revision)
                            .max()
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or("job definition revision exhausted")?
                    }
                };
                let baseline = if definition.cron.is_some()
                    && prior.is_none_or(|record| {
                        record.definition != definition || record.last_observed_minute.is_none()
                    }) {
                    Some(
                        i64::try_from(*now_epoch_secs / 60)
                            .map_err(|_| "cron time out of range")?,
                    )
                } else {
                    None
                };
                let last_observed_minute = prior.and_then(|r| r.last_observed_minute).max(baseline);
                self.definitions.insert(
                    key,
                    DefinitionRecord {
                        definition: definition.clone(),
                        revision,
                        last_observed_minute,
                    },
                );
                let Some(trigger) = trigger else {
                    return Ok(JobPlan::Recorded);
                };
                (definition, revision, trigger.clone(), *now_epoch_secs)
            }
            JobWrite::Fire {
                revision,
                minute,
                now_epoch_secs,
                ..
            } => {
                let record = self
                    .definitions
                    .get_mut(&key)
                    .ok_or("unknown job definition")?;
                if record.revision != *revision {
                    return Err("stale job definition revision".into());
                }
                let cron = record
                    .definition
                    .cron
                    .as_ref()
                    .ok_or("job has no cron schedule")?;
                if *minute < 0
                    || *minute
                        != i64::try_from(*now_epoch_secs / 60)
                            .map_err(|_| "cron time out of range")?
                {
                    return Err("cron occurrence must name the observed UTC minute".into());
                }
                if record
                    .last_observed_minute
                    .is_some_and(|seen| seen >= *minute)
                {
                    return Ok(JobPlan::Recorded);
                }
                let at = time::OffsetDateTime::from_unix_timestamp(
                    i64::try_from(*now_epoch_secs).map_err(|_| "cron time out of range")?,
                )
                .map_err(|_| "cron time out of range")?;
                if !super::cron::CronSchedule::parse(&cron.expression)
                    .map_err(|e| e.to_string())?
                    .matches(at)
                {
                    return Err("cron occurrence does not match the schedule".into());
                }
                record.last_observed_minute = Some(*minute);
                if self.runs.iter().any(|(id, run)| {
                    run.namespace == *namespace
                        && run.name == *name
                        && ((!run.replay_unknown && !run.unknown_owners.is_empty())
                            || (cron.overlap == OverlapPolicy::Forbid && active.contains(id)))
                }) {
                    return Ok(JobPlan::Recorded);
                }
                (
                    record.definition.clone(),
                    *revision,
                    RunTrigger::Cron { minute: *minute },
                    *now_epoch_secs,
                )
            }
        };
        if self.runs.len() >= MAX_JOB_RUNS {
            return Err("job run provenance limit reached".into());
        }
        let run = RunRecord {
            name: name.clone(),
            namespace: namespace.clone(),
            revision,
            trigger,
            definition_digest: definition.digest()?,
            replay_unknown: definition.replay_unknown,
            unknown_owners: BTreeSet::new(),
        };
        Ok(JobPlan::Register {
            definition: Box::new(definition),
            run,
            now_epoch_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::job::JobSpec;
    use crate::meat::task_array_store::{TaskArrayApplied, TaskArrayWrite, TaskArrays};

    fn definition(count: u32, schedule: Option<&str>) -> JobDefinition {
        JobDefinition {
            template: toml::from_str::<JobSpec>("runtime = 'process'\nexec = '/bin/true'").unwrap(),
            tasks: crate::meat::task_array::TaskArraySpec::with_count(count),
            cron: schedule.map(|expression| CronPolicy {
                expression: expression.into(),
                overlap: OverlapPolicy::Forbid,
                missed: MissedRunPolicy::Skip,
            }),
            replay_unknown: false,
        }
    }

    fn put(definition: JobDefinition, trigger: Option<RunTrigger>) -> TaskArrayWrite {
        TaskArrayWrite::Job(Box::new(JobWrite::Put {
            name: "cleanup".into(),
            namespace: "default".into(),
            definition: Box::new(definition),
            trigger,
            now_epoch_secs: 120,
        }))
    }

    fn fire(revision: u64, minute: i64) -> TaskArrayWrite {
        TaskArrayWrite::Job(Box::new(JobWrite::Fire {
            name: "cleanup".into(),
            namespace: "default".into(),
            revision,
            minute,
            now_epoch_secs: u64::try_from(minute).unwrap() * 60,
        }))
    }

    fn apply(store: &mut TaskArrays, write: TaskArrayWrite, next: &mut u64) -> TaskArrayApplied {
        store
            .apply(&write, || {
                *next += 1;
                *next
            })
            .unwrap()
    }

    #[test]
    fn deployment_hooks_preserve_the_single_attempt_side_effect_policy() {
        let hook: JobSpec =
            toml::from_str("runtime='process'\nexec='/bin/true'\nrun_before=['app.web']").unwrap();
        assert_eq!(JobDefinition::from_spec(hook).tasks.max_attempts, 1);
    }

    #[test]
    fn observed_schedule_time_skips_missed_minutes_even_after_clock_rollback() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        apply(
            &mut store,
            TaskArrayWrite::CronObserve { minute: 10 },
            &mut next,
        );
        assert_eq!(
            apply(&mut store, fire(1, 3), &mut next),
            TaskArrayApplied::JobRecorded
        );
        apply(
            &mut store,
            TaskArrayWrite::CronObserve { minute: 2 },
            &mut next,
        );
        assert_eq!(
            apply(&mut store, fire(1, 4), &mut next),
            TaskArrayApplied::JobRecorded
        );
        assert_eq!(next, 0);
        assert_eq!(
            apply(&mut store, fire(1, 10), &mut next),
            TaskArrayApplied::Registered { batch_id: 1 }
        );
    }

    #[test]
    fn allowing_overlap_does_not_replay_unknown_side_effects_through_a_new_cron_occurrence() {
        use crate::meat::{NodeId, index_set::IndexRangeSet};
        let mut store = TaskArrays::default();
        let mut next = 0;
        let mut definition = definition(1, Some("* * * * *"));
        definition.cron.as_mut().unwrap().overlap = OverlapPolicy::Allow;
        apply(&mut store, put(definition, None), &mut next);
        apply(&mut store, fire(1, 3), &mut next);
        let node = NodeId::new("worker");
        apply(
            &mut store,
            TaskArrayWrite::Sync {
                batch_id: 1,
                now_epoch_secs: 180,
                results: vec![],
                grants: vec![(node.clone(), IndexRangeSet::from_range(0..=0))],
            },
            &mut next,
        );
        apply(
            &mut store,
            TaskArrayWrite::Unknown { batch_id: 1, node },
            &mut next,
        );
        assert_eq!(
            apply(&mut store, fire(1, 4), &mut next),
            TaskArrayApplied::JobRecorded
        );
        assert_eq!(next, 1);
        assert_eq!(
            store
                .jobs()
                .definition("default", "cleanup")
                .unwrap()
                .last_observed_minute,
            Some(4)
        );
    }

    #[test]
    fn loss_of_an_ordinary_owner_requires_acknowledged_replay() {
        use crate::meat::{NodeId, index_set::IndexRangeSet};
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(
                definition(1, None),
                Some(RunTrigger::Manual {
                    request_id: "side-effect".into(),
                }),
            ),
            &mut next,
        );
        let owner = NodeId::new("worker");
        let mut chunks = IndexRangeSet::new();
        chunks.insert(0);
        apply(
            &mut store,
            TaskArrayWrite::Sync {
                batch_id: 1,
                now_epoch_secs: 120,
                results: vec![],
                grants: vec![(owner.clone(), chunks)],
            },
            &mut next,
        );
        let before = store.clone();
        assert!(
            store
                .apply(
                    &TaskArrayWrite::Requeue {
                        batch_id: 1,
                        node: owner.clone(),
                        now_epoch_secs: 121
                    },
                    || panic!("no allocation")
                )
                .is_err()
        );
        assert_eq!(store, before);
        apply(
            &mut store,
            TaskArrayWrite::Unknown {
                batch_id: 1,
                node: owner.clone(),
            },
            &mut next,
        );
        assert!(store.jobs().run(1).unwrap().unknown_owners.contains(&owner));
        let grant_digest = store.owner_fingerprint(1, &owner).unwrap();
        apply(
            &mut store,
            TaskArrayWrite::Replay {
                batch_id: 1,
                node: owner,
                grant_digest,
                now_epoch_secs: 122,
            },
            &mut next,
        );
        assert!(store.jobs().run(1).unwrap().unknown_owners.is_empty());
        assert_eq!(
            store
                .get(1)
                .unwrap()
                .state
                .attempt_of(crate::meat::task_array::ChunkId(0)),
            2
        );
    }

    #[test]
    fn clock_rollback_before_the_first_occurrence_cannot_launch_pre_registration_work() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        assert_eq!(
            apply(&mut store, fire(1, 1), &mut next),
            TaskArrayApplied::JobRecorded
        );
        assert_eq!(next, 0);
        assert_eq!(
            apply(&mut store, fire(1, 2), &mut next),
            TaskArrayApplied::JobRecorded
        );
        assert_eq!(
            apply(&mut store, fire(1, 3), &mut next),
            TaskArrayApplied::Registered { batch_id: 1 }
        );
    }

    #[test]
    fn oversized_schedule_text_is_refused_before_any_catalogue_mutation() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let mut d = definition(1, Some("* * * * *"));
        d.cron.as_mut().unwrap().expression = format!("{}* * * * *", " ".repeat(32 * 1024));
        let before = store.clone();
        assert!(
            store
                .apply(&put(d, None), || {
                    next += 1;
                    next
                })
                .is_err()
        );
        assert_eq!(store, before);
        assert_eq!(next, 0);
    }

    #[test]
    fn a_replayed_run_or_skipped_occurrence_needs_no_counter_capacity() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let write = put(
            definition(1, None),
            Some(RunTrigger::Manual {
                request_id: "first".into(),
            }),
        );
        apply(&mut store, write.clone(), &mut next);
        assert_eq!(store.registration_ids(&write).unwrap(), 0);
        let fresh = put(
            definition(1, None),
            Some(RunTrigger::Manual {
                request_id: "second".into(),
            }),
        );
        assert_eq!(store.registration_ids(&fresh).unwrap(), 1);
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        assert_eq!(store.registration_ids(&fire(2, 3)).unwrap(), 0);
    }

    #[test]
    fn an_allowed_overlap_creates_independent_runs_with_the_same_definition() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let mut d = definition(2, Some("* * * * *"));
        d.cron.as_mut().unwrap().overlap = OverlapPolicy::Allow;
        apply(&mut store, put(d, None), &mut next);
        apply(&mut store, fire(1, 3), &mut next);
        apply(&mut store, fire(1, 4), &mut next);
        assert_eq!(next, 2);
        assert_eq!(
            store.jobs().run(1).unwrap().revision,
            store.jobs().run(2).unwrap().revision
        );
        assert_ne!(
            store.jobs().run(1).unwrap().trigger,
            store.jobs().run(2).unwrap().trigger
        );
    }

    #[test]
    fn admission_at_the_active_run_limit_cannot_partly_update_the_definition() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        for i in 0..crate::meat::task_array_store::MAX_ACTIVE_ARRAYS {
            apply(
                &mut store,
                put(
                    definition(1, None),
                    Some(RunTrigger::Manual {
                        request_id: format!("request-{i}"),
                    }),
                ),
                &mut next,
            );
        }
        let before = store.clone();
        let write = put(
            definition(12, None),
            Some(RunTrigger::Manual {
                request_id: "overflow".into(),
            }),
        );
        assert!(
            store
                .apply(&write, || {
                    next += 1;
                    next
                })
                .is_err()
        );
        assert_eq!(store, before);
        assert_eq!(
            next,
            crate::meat::task_array_store::MAX_ACTIVE_ARRAYS as u64
        );
    }

    #[test]
    fn a_definition_update_retains_its_schedule_cursor_and_the_runs_replay_policy() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        apply(&mut store, fire(1, 3), &mut next);
        let mut updated = definition(2, Some("*/2 * * * *"));
        updated.replay_unknown = true;
        apply(&mut store, put(updated, None), &mut next);
        let record = store.jobs().definition("default", "cleanup").unwrap();
        assert_eq!(record.last_observed_minute, Some(3));
        assert_eq!(record.revision, 2);
        assert!(!store.jobs().run(1).unwrap().replay_unknown);
    }

    #[test]
    fn a_nonmatching_cron_occurrence_has_no_effect() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("0 * * * *")), None),
            &mut next,
        );
        let before = store.clone();
        assert!(
            store
                .apply(&fire(1, 3), || {
                    next += 1;
                    next
                })
                .is_err()
        );
        assert_eq!(store, before);
        assert_eq!(next, 0);
    }

    #[test]
    fn result_pruning_keeps_the_occurrence_fence() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        apply(&mut store, fire(1, 3), &mut next);
        apply(
            &mut store,
            TaskArrayWrite::Cancel {
                batch_id: 1,
                now_epoch_secs: 181,
            },
            &mut next,
        );
        let mut another = put(
            definition(1, None),
            Some(RunTrigger::Manual {
                request_id: "later".into(),
            }),
        );
        let TaskArrayWrite::Job(write) = &mut another else {
            unreachable!()
        };
        let JobWrite::Put {
            name,
            now_epoch_secs,
            ..
        } = write.as_mut()
        else {
            unreachable!()
        };
        *name = "other".into();
        *now_epoch_secs = 10_000;
        apply(&mut store, another, &mut next);
        assert!(store.get(1).is_none());
        assert!(store.jobs().run(1).is_none());
        let reopened: TaskArrays =
            serde_json::from_value(serde_json::to_value(&store).unwrap()).unwrap();
        store = reopened;
        apply(&mut store, fire(1, 3), &mut next);
        assert_eq!(next, 2);
    }

    #[test]
    fn reopening_refuses_a_malformed_definition_revision() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(&mut store, put(definition(1, None), None), &mut next);
        let mut encoded = serde_json::to_value(&store).unwrap();
        encoded["jobs"]["definitions"]["default/cleanup"]["revision"] = serde_json::json!(0);
        assert!(serde_json::from_value::<TaskArrays>(encoded).is_err());
    }

    #[test]
    fn an_omitted_task_policy_is_a_singleton() {
        let d: JobDefinition =
            serde_json::from_str(r#"{"template":{"exec":"/bin/true"}}"#).unwrap();
        assert_eq!(d.tasks.count, 1);
        assert!(!d.replay_unknown);
    }

    #[test]
    fn a_duplicate_manual_trigger_reuses_the_original_run_without_spending_an_id() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let write = put(
            definition(1, None),
            Some(RunTrigger::Manual {
                request_id: "first".into(),
            }),
        );
        assert_eq!(
            apply(&mut store, write.clone(), &mut next),
            TaskArrayApplied::Registered { batch_id: 1 }
        );
        assert_eq!(
            apply(&mut store, write, &mut next),
            TaskArrayApplied::Registered { batch_id: 1 }
        );
        assert_eq!(next, 1);
        assert_eq!(store.get(1).unwrap().state.spec.count, 1);
        assert_eq!(store.jobs().run(1).unwrap().revision, 1);
    }

    #[test]
    fn reusing_a_trigger_for_changed_work_is_refused_atomically() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let trigger = RunTrigger::Manual {
            request_id: "first".into(),
        };
        apply(
            &mut store,
            put(definition(1, None), Some(trigger.clone())),
            &mut next,
        );
        let before = store.clone();
        assert!(
            store
                .apply(&put(definition(2, None), Some(trigger)), || {
                    next += 1;
                    next
                })
                .is_err()
        );
        assert_eq!(store, before);
        assert_eq!(next, 1);
    }

    #[test]
    fn a_definition_update_never_changes_an_admitted_run() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(
                definition(1, None),
                Some(RunTrigger::Manual {
                    request_id: "first".into(),
                }),
            ),
            &mut next,
        );
        apply(&mut store, put(definition(12, None), None), &mut next);
        assert_eq!(store.get(1).unwrap().state.spec.count, 1);
        assert_eq!(store.jobs().run(1).unwrap().revision, 1);
        assert_eq!(
            store
                .jobs()
                .definition("default", "cleanup")
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn cron_recovery_and_clock_rollback_cannot_repeat_an_occurrence() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(8, Some("* * * * *")), None),
            &mut next,
        );
        apply(&mut store, fire(1, 3), &mut next);
        let mut reopened: TaskArrays =
            serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        apply(&mut reopened, fire(1, 3), &mut next);
        apply(&mut reopened, fire(1, 2), &mut next);
        assert_eq!(next, 1);
        assert_eq!(
            reopened
                .jobs()
                .definition("default", "cleanup")
                .unwrap()
                .last_observed_minute,
            Some(3)
        );
        assert_eq!(reopened.get(1).unwrap().state.spec.count, 8);
    }

    #[test]
    fn a_forbidden_overlap_is_durably_skipped() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        apply(&mut store, fire(1, 3), &mut next);
        apply(&mut store, fire(1, 4), &mut next);
        assert_eq!(next, 1);
        assert_eq!(
            store
                .jobs()
                .definition("default", "cleanup")
                .unwrap()
                .last_observed_minute,
            Some(4)
        );
        apply(
            &mut store,
            TaskArrayWrite::Cancel {
                batch_id: 1,
                now_epoch_secs: 241,
            },
            &mut next,
        );
        apply(&mut store, fire(1, 4), &mut next);
        assert_eq!(next, 1);
        apply(&mut store, fire(1, 5), &mut next);
        assert_eq!(next, 2);
    }

    #[test]
    fn a_stale_revision_cannot_fire_after_a_schedule_update() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            put(definition(1, Some("* * * * *")), None),
            &mut next,
        );
        apply(
            &mut store,
            put(definition(2, Some("* * * * *")), None),
            &mut next,
        );
        let before = store.clone();
        assert!(
            store
                .apply(&fire(1, 3), || {
                    next += 1;
                    next
                })
                .is_err()
        );
        assert_eq!(store, before);
        assert_eq!(next, 0);
    }

    #[test]
    fn a_hook_operation_is_a_run_trigger_in_the_same_store() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let trigger = RunTrigger::Hook {
            operation_id: "deployment".into(),
        };
        apply(
            &mut store,
            put(definition(1, None), Some(trigger.clone())),
            &mut next,
        );
        assert_eq!(store.jobs().run(1).unwrap().trigger, trigger);
        assert_eq!(store.get(1).unwrap().state.spec.count, 1);
    }

    #[test]
    fn toml_job_timeout_reaches_the_definition() {
        let config = crate::config::Config::parse(
            "[job.cleanup]\nimage = 'cleanup:v1'\nschedule = '0 3 * * *'\nmax_attempts = 2\ntask_timeout_secs = 30\noverlap = 'allow'\nreplay_unknown = true\n",
        )
        .unwrap();
        config.validate().unwrap();
        let definition = JobDefinition::from_spec(config.job["cleanup"].clone());
        assert_eq!(definition.tasks.task_timeout_secs, 30);
        assert_eq!(definition.tasks.max_attempts, 2);
        assert!(definition.replay_unknown);
        let cron = definition.cron.as_ref().unwrap();
        assert_eq!(cron.overlap, OverlapPolicy::Allow);
        // The policy moved out of the template, so the template is a valid
        // execution template and the definition as a whole validates.
        assert!(!definition.template.has_run_policy());
        definition.validate().unwrap();
    }

    #[test]
    fn toml_job_without_policy_keeps_todays_defaults() {
        let config = crate::config::Config::parse(
            "[job.once]\nimage = 'once:v1'\n\n[job.nightly]\nimage = 'once:v1'\nschedule = '0 3 * * *'\n\n[job.migrate]\nimage = 'once:v1'\nrun_before = ['app.web']\n\n[app.web]\nimage = 'web:v1'\n",
        )
        .unwrap();
        config.validate().unwrap();
        let once = JobDefinition::from_spec(config.job["once"].clone());
        assert_eq!(
            (once.tasks.max_attempts, once.tasks.task_timeout_secs),
            (4, 0)
        );
        assert!(!once.replay_unknown);
        let nightly = JobDefinition::from_spec(config.job["nightly"].clone());
        assert_eq!(nightly.cron.unwrap().overlap, OverlapPolicy::Forbid);
        let hook = JobDefinition::from_spec(config.job["migrate"].clone());
        assert_eq!(
            (hook.tasks.max_attempts, hook.tasks.task_timeout_secs),
            (1, 600)
        );
        assert!(!hook.replay_unknown);
    }

    #[test]
    fn hook_refuses_replay_unknown() {
        let config = crate::config::Config::parse(
            "[job.migrate]\nimage = 'once:v1'\nrun_before = ['app.web']\nreplay_unknown = true\n\n[app.web]\nimage = 'web:v1'\n",
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("replay_unknown"), "{error}");
        // A hook may still choose its attempts and deadline.
        let config = crate::config::Config::parse(
            "[job.migrate]\nimage = 'once:v1'\nrun_before = ['app.web']\nmax_attempts = 3\ntask_timeout_secs = 0\n\n[app.web]\nimage = 'web:v1'\n",
        )
        .unwrap();
        config.validate().unwrap();
        let hook = JobDefinition::from_spec(config.job["migrate"].clone());
        assert_eq!(
            (hook.tasks.max_attempts, hook.tasks.task_timeout_secs),
            (3, 0)
        );
    }

    #[test]
    fn array_templates_refuse_run_policy_fields() {
        let mut definition = definition(4, None);
        definition.template.task_timeout_secs = Some(30);
        let error = definition.validate().unwrap_err();
        assert!(error.contains("task_timeout_secs"), "{error}");
    }
}

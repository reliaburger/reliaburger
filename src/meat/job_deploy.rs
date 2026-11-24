//! Durable deploy intent pins common hook runs until app publication is authorised.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Terminal deployment receipt, retained independently of pruned task results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Durable intent and accepted run identities, independent of the submitting leader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentRecord {
    /// Encrypted, image-bound configuration accepted before any hook starts.
    pub config: Box<Config>,
    /// The image each job in `config` declared, by job name, where
    /// admission bound or pinned it to another (#1322).
    pub declared_images: BTreeMap<String, String>,
    /// Hook names to common run identities.
    pub hook_runs: BTreeMap<String, u64>,
    /// Ordinary job names to common run identities; schedules create later runs.
    pub job_runs: BTreeMap<String, u64>,
    /// Apps were positively published before ordinary jobs were admitted.
    pub apps_committed: bool,
    /// When cancellation was requested, if it was. Cancellation wins over
    /// later success and prevents app publication.
    pub cancelled_at: Option<u64>,
    /// Terminal receipts remain bounded for idempotency and disconnected clients.
    pub completed: bool,
    /// Immutable terminal outcome and settlement time.
    pub outcome: Option<DeploymentOutcome>,
    pub finished_at: Option<u64>,
    /// Admission time supplied in the write.
    pub submitted_at_epoch_secs: u64,
}
impl DeploymentRecord {
    /// Whether cancellation was requested.
    pub fn cancelled(&self) -> bool {
        self.cancelled_at.is_some()
    }
    /// Workload ownership survives leader changes and unknown execution outcomes.
    pub fn blocks(&self, name: &str, namespace: &str) -> bool {
        if self.completed {
            return false;
        }
        (!self.apps_committed
            && self
                .config
                .app
                .get(name)
                .is_some_and(|spec| spec.namespace.as_deref().unwrap_or("default") == namespace))
            || self.config.job.get(name).is_some_and(|spec| {
                spec.namespace.as_deref().unwrap_or("default") == namespace
                    && (!self.apps_committed || self.job_runs.contains_key(name))
            })
    }
    /// Run identities pinned until the deployment is positively settled.
    pub fn runs(&self) -> impl Iterator<Item = u64> + '_ {
        self.hook_runs
            .values()
            .chain(self.job_runs.values())
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::Config;
    use crate::meat::{
        NodeId,
        index_set::IndexRangeSet,
        task_array::ChunkId,
        task_array_state::ChunkResult,
        task_array_store::{TaskArrayWrite, TaskArrays},
    };

    const OP: &str = "0123456789abcdef0123456789abcdef";
    fn config() -> Config {
        Config::parse("[app.web]\nimage='web:v1'\n[job.migrate]\nruntime='process'\nexec='/bin/true'\nrun_before=['app.web']\n[job.notify]\nruntime='process'\nexec='/bin/true'").unwrap()
    }
    fn apply(store: &mut TaskArrays, write: TaskArrayWrite, next: &mut u64) {
        store
            .apply(&write, || {
                *next += 1;
                *next
            })
            .unwrap();
    }
    fn finish(store: &mut TaskArrays, id: u64, succeeded: bool, next: &mut u64) {
        let worker = NodeId::new("worker");
        apply(
            store,
            TaskArrayWrite::Sync {
                batch_id: id,
                now_epoch_secs: 10,
                results: vec![],
                grants: vec![(worker.clone(), IndexRangeSet::from_range(0..=0))],
            },
            next,
        );
        apply(
            store,
            TaskArrayWrite::Sync {
                batch_id: id,
                now_epoch_secs: 11,
                grants: vec![],
                results: vec![(
                    worker,
                    ChunkResult {
                        chunk: ChunkId(0),
                        attempt: 1,
                        succeeded: u32::from(succeeded),
                        failed_count: u32::from(!succeeded),
                        failed_indices: if succeeded {
                            IndexRangeSet::new()
                        } else {
                            IndexRangeSet::from_range(0..=0)
                        },
                        not_run: 0,
                        retried: 0,
                        retry_causes: Default::default(),
                        duration_counts: [0; 16],
                    },
                )],
            },
            next,
        );
    }
    /// A deployment of two jobs whose images admission bound: a hook and
    /// an ordinary job, with the tags they declared (#1322).
    fn bound_deployment() -> (Config, BTreeMap<String, String>) {
        let digest = format!("sha256:{}", "d".repeat(64));
        let config = Config::parse(&format!(
            "[app.web]\nimage='web:v1'\n\
             [job.migrate]\nimage='migrate:v1@{digest}'\ncommand=['true']\nrun_before=['app.web']\n\
             [job.notify]\nimage='notify:v1@{digest}'\ncommand=['true']"
        ))
        .unwrap();
        let declared = BTreeMap::from([
            ("migrate".to_string(), "migrate:v1".to_string()),
            ("notify".to_string(), "notify:v1".to_string()),
        ]);
        (config, declared)
    }

    /// #1322: the hook a deployment admits at once and the job it admits
    /// once the apps commit both keep the image they declared.
    #[test]
    fn a_deployment_admits_its_jobs_with_the_images_they_declared() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let (config, declared) = bound_deployment();
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config),
                declared_images: declared,
                now_epoch_secs: 10,
            },
            &mut next,
        );
        let declared_image = |store: &TaskArrays, name: &str| {
            store
                .jobs()
                .definition("default", name)
                .map(|record| record.definition.declared_spec().image)
        };
        assert_eq!(
            declared_image(&store, "migrate"),
            Some(Some("migrate:v1".into()))
        );
        assert_eq!(declared_image(&store, "notify"), None);
        let hook = store.deployments().next().unwrap().1.hook_runs["migrate"];
        finish(&mut store, hook, true, &mut next);
        apply(
            &mut store,
            TaskArrayWrite::DeployCommitted {
                operation_id: OP.into(),
                now_epoch_secs: 20,
            },
            &mut next,
        );
        assert_eq!(
            declared_image(&store, "notify"),
            Some(Some("notify:v1".into()))
        );
    }

    /// #1322: a declared image names a job the same write carries, and a
    /// deployment's identity covers what its jobs declared.
    #[test]
    fn declared_images_belong_to_the_jobs_in_the_write() {
        let mut store = TaskArrays::default();
        let (config, mut declared) = bound_deployment();
        let mut ids = || 1;
        let mut ghost = declared.clone();
        ghost.insert("ghost".into(), "ghost:v1".into());
        let begin = |declared_images| TaskArrayWrite::DeployBegin {
            operation_id: OP.into(),
            config: Box::new(config.clone()),
            declared_images,
            now_epoch_secs: 10,
        };
        assert!(store.apply(&begin(ghost), &mut ids).is_err());
        let mut next = 0;
        apply(&mut store, begin(declared.clone()), &mut next);
        declared.remove("notify");
        assert!(store.apply(&begin(declared), &mut ids).is_err());

        let jobs = vec![("first".to_string(), config.job["notify"].clone())];
        let register = TaskArrayWrite::RegisterJobs {
            request_id: "batch".into(),
            jobs,
            declared_images: BTreeMap::from([("second".to_string(), "notify:v1".to_string())]),
            submitted_at_epoch_secs: 10,
        };
        assert!(store.apply(&register, &mut ids).is_err());
    }

    #[test]
    fn reopening_refuses_missing_hook_identity_or_outcome_state() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        let mut wire = serde_json::to_value(&store).unwrap();
        wire["deployments"][OP]["hook_runs"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<TaskArrays>(wire).is_err(),
            "missing hook must never open the app gate"
        );
        let mut wire = serde_json::to_value(&store).unwrap();
        wire["arrays"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<TaskArrays>(wire).is_err(),
            "pending deployments must retain exact outcomes"
        );
    }
    #[test]
    fn expired_finite_groups_free_provenance_capacity_before_new_admission() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let jobs: Vec<_> = (0..64)
            .map(|index| (format!("job-{index}"), config().job["notify"].clone()))
            .collect();
        for group in 0..8 {
            let result = store
                .apply(
                    &TaskArrayWrite::RegisterJobs {
                        request_id: format!("group-{group}"),
                        jobs: jobs.clone(),
                        declared_images: Default::default(),
                        submitted_at_epoch_secs: 10,
                    },
                    || {
                        next += 1;
                        next
                    },
                )
                .unwrap();
            let crate::meat::task_array_store::TaskArrayApplied::Registered { batch_id } = result
            else {
                panic!("no parent");
            };
            let ids: Vec<_> = store
                .manifest(batch_id)
                .unwrap()
                .cohorts
                .iter()
                .map(|(_, id)| *id)
                .collect();
            for id in ids {
                finish(&mut store, id, true, &mut next);
            }
        }
        assert_eq!(store.jobs().runs().count(), 512);
        apply(
            &mut store,
            TaskArrayWrite::RegisterJobs {
                request_id: "after-expiry".into(),
                jobs: vec![("after".into(), config().job["notify"].clone())],
                declared_images: Default::default(),
                submitted_at_epoch_secs: 10_000,
            },
            &mut next,
        );
        assert_eq!(store.jobs().runs().count(), 1);
    }

    #[test]
    fn finite_groups_are_atomic_common_singletons_with_idempotent_parent_receipts() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let jobs = vec![
            ("first".to_string(), config().job["notify"].clone()),
            ("second".to_string(), config().job["notify"].clone()),
        ];
        let write = TaskArrayWrite::RegisterJobs {
            request_id: OP.into(),
            jobs: jobs.clone(),
            declared_images: Default::default(),
            submitted_at_epoch_secs: 10,
        };
        let result = store
            .apply(&write, || {
                next += 1;
                next
            })
            .unwrap();
        let crate::meat::task_array_store::TaskArrayApplied::Registered { batch_id } = result
        else {
            panic!("no parent");
        };
        assert_eq!(store.manifest(batch_id).unwrap().cohorts.len(), 2);
        assert_eq!(store.jobs().runs().count(), 2);
        assert!(store.jobs().runs().all(|(_, run)| !run.replay_unknown));
        assert_eq!(store.registration_ids(&write).unwrap(), 0);
        assert_eq!(
            store
                .apply(&write, || panic!("replay cannot allocate"))
                .unwrap(),
            result
        );
        let before = store.clone();
        let mut invalid = jobs;
        invalid[1].1.exec = None;
        assert!(
            store
                .apply(
                    &TaskArrayWrite::RegisterJobs {
                        request_id: "abcdef0123456789abcdef0123456789".into(),
                        jobs: invalid,
                        declared_images: Default::default(),
                        submitted_at_epoch_secs: 10
                    },
                    || panic!("refusal cannot allocate")
                )
                .is_err()
        );
        assert_eq!(store, before);
    }
    #[test]
    fn stopping_a_definition_atomically_disables_cron_and_cancels_its_runs() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        finish(&mut store, 1, true, &mut next);
        apply(
            &mut store,
            TaskArrayWrite::DeployCommitted {
                operation_id: OP.into(),
                now_epoch_secs: 12,
            },
            &mut next,
        );
        apply(
            &mut store,
            TaskArrayWrite::StopDefinition {
                name: "notify".into(),
                namespace: "default".into(),
                forget: false,
                now_epoch_secs: 13,
            },
            &mut next,
        );
        assert_eq!(
            store.get(2).unwrap().state.status(),
            crate::meat::task_array_state::TaskArrayStatus::Cancelled
        );
        assert!(
            store
                .jobs()
                .definition("default", "notify")
                .unwrap()
                .definition
                .cron
                .is_none()
        );
    }

    #[test]
    fn a_deploy_admits_hooks_first_and_ordinary_jobs_only_after_accepted_success() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        assert_eq!(next, 1);
        assert!(store.deployment(OP).unwrap().blocks("web", "default"));
        let before = store.clone();
        assert!(
            store
                .apply(
                    &TaskArrayWrite::DeployCommitted {
                        operation_id: OP.into(),
                        now_epoch_secs: 12
                    },
                    || panic!("cannot allocate before success")
                )
                .is_err()
        );
        assert_eq!(store, before);
        finish(&mut store, 1, true, &mut next);
        apply(
            &mut store,
            TaskArrayWrite::DeployCommitted {
                operation_id: OP.into(),
                now_epoch_secs: 12,
            },
            &mut next,
        );
        assert_eq!(next, 2);
        let deployment = store.deployment(OP).unwrap();
        assert!(deployment.apps_committed);
        assert!(!deployment.blocks("web", "default"));
        assert!(deployment.blocks("notify", "default"));
        assert!(
            store
                .apply(
                    &TaskArrayWrite::DeployRelease {
                        operation_id: OP.into()
                    },
                    || panic!("no allocation")
                )
                .is_err()
        );
        finish(&mut store, 2, true, &mut next);
        apply(
            &mut store,
            TaskArrayWrite::DeployRelease {
                operation_id: OP.into(),
            },
            &mut next,
        );
        assert!(store.deployment(OP).is_none());
    }
    #[test]
    fn failed_hooks_never_authorise_app_publication() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        finish(&mut store, 1, false, &mut next);
        assert!(
            store
                .apply(
                    &TaskArrayWrite::DeployCommitted {
                        operation_id: OP.into(),
                        now_epoch_secs: 12
                    },
                    || panic!("no allocation")
                )
                .is_err()
        );
        apply(
            &mut store,
            TaskArrayWrite::DeployRelease {
                operation_id: OP.into(),
            },
            &mut next,
        );
        assert!(store.deployment(OP).is_none());
    }
    #[test]
    fn cancellation_is_durable_and_cannot_race_success_into_publication() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        finish(&mut store, 1, true, &mut next);
        apply(
            &mut store,
            TaskArrayWrite::DeployCancel {
                operation_id: OP.into(),
                now_epoch_secs: 12,
            },
            &mut next,
        );
        let reopened: TaskArrays =
            serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        store = reopened;
        assert!(
            store
                .apply(
                    &TaskArrayWrite::DeployCommitted {
                        operation_id: OP.into(),
                        now_epoch_secs: 12
                    },
                    || panic!("no allocation")
                )
                .is_err()
        );
    }
    #[test]
    fn a_repeated_deploy_preserves_runs_and_refuses_changed_content() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        let write = TaskArrayWrite::DeployBegin {
            operation_id: OP.into(),
            config: Box::new(config()),
            declared_images: Default::default(),
            now_epoch_secs: 10,
        };
        apply(&mut store, write.clone(), &mut next);
        assert_eq!(store.registration_ids(&write).unwrap(), 0);
        let before = store.clone();
        apply(&mut store, write, &mut next);
        assert_eq!(store, before);
        let mut changed = config();
        changed.app.get_mut("web").unwrap().image = Some("web:v2".into());
        assert!(
            store
                .apply(
                    &TaskArrayWrite::DeployBegin {
                        operation_id: OP.into(),
                        config: Box::new(changed),
                        declared_images: Default::default(),
                        now_epoch_secs: 10
                    },
                    || panic!("no allocation")
                )
                .is_err()
        );
        assert_eq!(store, before);
    }
    #[test]
    fn pinned_hook_results_survive_normal_result_pruning() {
        let mut store = TaskArrays::default();
        let mut next = 0;
        apply(
            &mut store,
            TaskArrayWrite::DeployBegin {
                operation_id: OP.into(),
                config: Box::new(config()),
                declared_images: Default::default(),
                now_epoch_secs: 10,
            },
            &mut next,
        );
        finish(&mut store, 1, true, &mut next);
        store.prune(10_000);
        assert!(store.get(1).is_some());
        assert!(store.deployment(OP).is_some());
    }
}

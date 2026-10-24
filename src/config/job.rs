/// Job specification — a run-to-completion task.
///
/// Jobs can run inside a container (with `image`) or as a host process
/// (`exec`/`script`, Phase 8). They support cron scheduling and
/// dependency ordering via `run_before`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{EnvValue, PidsLimit, ResourceRange};

/// The job execution backend. Image containers are the default; other backends are explicit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum JobRuntime {
    /// A new owned container generation for each attempt.
    #[default]
    Runc,
    /// An explicitly allowlisted host process, without a container image.
    Process,
    /// A separate command process inside a compatible reusable container.
    SharedRunc,
}

impl JobRuntime {
    fn is_fresh(&self) -> bool {
        *self == Self::Runc
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// Explicit execution backend; runc is the default.
    #[serde(default, skip_serializing_if = "JobRuntime::is_fresh")]
    pub runtime: JobRuntime,
    /// OCI image reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Command and arguments to run inside the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Cron schedule (UTC), e.g. "0 3 * * *".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// Dependencies — job/app names that must complete before this runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_before: Vec<String>,
    /// Memory request-limit range.
    #[serde(
        default,
        with = "super::types::memory_range",
        skip_serializing_if = "Option::is_none"
    )]
    pub memory: Option<ResourceRange>,
    /// CPU request-limit range.
    #[serde(
        default,
        with = "super::types::cpu_range",
        skip_serializing_if = "Option::is_none"
    )]
    pub cpu: Option<ResourceRange>,
    /// Most processes and threads one command may hold (`pids.max`, 1 to
    /// [`PidsLimit::MAX`]). Omitted: 256 for a reused executor's commands,
    /// no limit for a fresh container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<PidsLimit>,
    /// Environment variables (plain or encrypted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// Namespace this job belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Host binary path (Phase 8: process workloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<PathBuf>,
    /// Inline script content (Phase 8: process workloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Apps in other namespaces allowed to connect to this job's addresses,
    /// with the same meaning as an app's `[firewall]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firewall: Option<super::app::FirewallSpec>,
    /// Destinations this job may reach outside the cluster, with the same
    /// meaning as an app's `[egress]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<super::app::EgressSpec>,

    // Run policy. The names match the manifest and JSON API fields, and
    // `JobDefinition::from_spec` moves them out of the template.
    /// Attempts per run before it counts as failed (1–10). Omitted: 4, or 1
    /// for a `run_before` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u8>,
    /// Per-attempt wall-clock limit in seconds; 0 means no deadline. Omitted:
    /// no deadline, or 600 for a `run_before` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_timeout_secs: Option<u32>,
    /// Whether a cron occurrence may start while an earlier run is still
    /// active. Only valid with `schedule`; omitted means `forbid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapPolicy>,
    /// Replay automatically after a worker is lost with an unknown outcome,
    /// instead of waiting for `relish batch replay`. Hooks refuse it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replay_unknown: bool,
    /// The template and `[use.*]` that made this job, when one did (#1124).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<super::template::Provenance>,
}

/// Whether a matching cron occurrence may overlap an earlier run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    /// Advance the occurrence cursor without launching while a run remains active.
    #[default]
    Forbid,
    /// Admit another run, still subject to the cluster's bounded capacity.
    Allow,
}

impl JobSpec {
    /// Whether this job runs a host command rather than a container.
    ///
    /// For a validated spec that is exactly `runtime = "process"`. An
    /// unvalidated one that names a host command anywhere counts as host too,
    /// so routing and the `host-exec` permission check never treat a host
    /// command as a container, whether or not validation ran first.
    pub fn is_host(&self) -> bool {
        self.runtime == JobRuntime::Process || self.exec.is_some() || self.script.is_some()
    }

    /// Whether any run-policy field is set. Array templates refuse them,
    /// because an array carries its policy beside the template.
    pub fn has_run_policy(&self) -> bool {
        self.max_attempts.is_some()
            || self.task_timeout_secs.is_some()
            || self.overlap.is_some()
            || self.replay_unknown
    }

    /// Refuse fields that contradict the explicitly selected execution backend.
    pub fn validate_runtime(&self) -> Result<(), &'static str> {
        match self.runtime {
            JobRuntime::Process if self.image.is_some() => {
                Err("runtime=process refuses image; use exec or script")
            }
            JobRuntime::Process if self.exec.is_some() == self.script.is_some() => {
                Err("runtime=process requires exactly one of exec or script")
            }
            JobRuntime::Runc | JobRuntime::SharedRunc if self.image.is_none() => Err(
                "runtime=runc/shared-runc requires an image; host exec/script requires runtime=process",
            ),
            JobRuntime::Runc | JobRuntime::SharedRunc
                if self.exec.is_some() || self.script.is_some() =>
            {
                Err("container runtime refuses host exec/script; use runtime=process")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_jobs_are_named_by_runtime_and_unvalidated_host_fields_still_count() {
        let host: JobSpec = toml::from_str("runtime='process'\nscript='true'").unwrap();
        assert!(host.is_host());
        let image: JobSpec = toml::from_str("image='fixture:v1'").unwrap();
        assert!(!image.is_host());
        // Invalid, refused by validate_runtime; never routed as a container.
        let mislabelled: JobSpec = toml::from_str("exec='/bin/true'").unwrap();
        assert!(mislabelled.validate_runtime().is_err());
        assert!(mislabelled.is_host());
    }

    #[test]
    fn container_reuse_is_explicit_and_fresh_is_the_default() {
        let fresh: JobSpec = toml::from_str("image='fixture:v1'").unwrap();
        assert_eq!(fresh.runtime, JobRuntime::Runc);
        let reused: JobSpec = toml::from_str(
            "image='fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nruntime='shared-runc'",
        ).unwrap();
        assert_eq!(reused.runtime, JobRuntime::SharedRunc);
        let encoded = toml::to_string(&reused).unwrap();
        assert!(encoded.contains("runtime = \"shared-runc\""));
        assert!(!toml::to_string(&fresh).unwrap().contains("runtime"));
        assert!(
            toml::from_str::<JobSpec>("image='fixture:v1'\nruntime='resident-worker'").is_err()
        );
    }

    #[test]
    fn container_reuse_refuses_host_execution_before_admission() {
        let source = "[job.worker]\nexec='/bin/true'\nruntime='shared-runc'";
        let error = crate::config::Config::parse(source)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("shared-runc"));
    }

    #[test]
    fn explicit_job_runtime_selects_the_backend_without_guessing_from_fields() {
        for source in [
            "[job.worker]\nruntime='runc'\nimage='fixture:v1'",
            "[job.worker]\nruntime='shared-runc'\nimage='fixture:v1'",
            "[job.worker]\nruntime='process'\nexec='/bin/true'",
            "[job.worker]\nruntime='process'\nscript='true'",
        ] {
            crate::config::Config::parse(source)
                .unwrap()
                .validate()
                .unwrap();
        }
        for source in [
            "[job.worker]\nruntime='process'\nimage='fixture:v1'",
            "[job.worker]\nruntime='process'\nimage='fixture:v1'\nexec='/bin/true'",
            "[job.worker]\nruntime='runc'\nexec='/bin/true'",
            "[job.worker]\nruntime='shared-runc'\nscript='true'",
            "[job.worker]\nexec='/bin/true'",
        ] {
            assert!(
                crate::config::Config::parse(source)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn obsolete_isolation_field_is_refused_instead_of_selecting_a_backend() {
        assert!(
            crate::config::Config::parse(
                "[job.worker]\nimage='fixture:v1'\nisolation='reusable-container'"
            )
            .is_err()
        );
    }

    #[test]
    fn parse_minimal_job() {
        let toml_str = r#"
            image = "myapp:v1"
            command = ["npm", "run", "migrate"]
        "#;
        let j: JobSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(j.image.as_deref(), Some("myapp:v1"));
        assert_eq!(
            j.command.as_deref(),
            Some(&["npm".to_string(), "run".to_string(), "migrate".to_string()][..])
        );
        assert!(j.schedule.is_none());
        assert!(j.run_before.is_empty());
    }

    #[test]
    fn parse_job_with_schedule() {
        let toml_str = r#"
            image = "cleanup:latest"
            schedule = "0 3 * * *"
        "#;
        let j: JobSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(j.schedule.as_deref(), Some("0 3 * * *"));
    }

    #[test]
    fn parse_job_with_run_before() {
        let toml_str = r#"
            image = "myapp:v1"
            command = ["npm", "run", "migrate"]
            run_before = ["app.api", "app.web"]
        "#;
        let j: JobSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(j.run_before, vec!["app.api", "app.web"]);
    }

    #[test]
    fn job_accepts_egress_and_allow_from() {
        let job: JobSpec = toml::from_str(
            r#"
            image = "fetcher:v1"

            [firewall]
            allow_from = ["web", "frontend/scraper"]

            [egress]
            allow = ["api.example.com:443", "10.20.0.0/16:5432"]
        "#,
        )
        .unwrap();
        assert_eq!(
            job.firewall.as_ref().unwrap().allow_from,
            vec!["web", "frontend/scraper"]
        );
        assert_eq!(
            job.egress.as_ref().unwrap().allow,
            vec!["api.example.com:443", "10.20.0.0/16:5432"]
        );
        // Both survive the round trip into Raft desired state.
        let encoded = toml::to_string(&job).unwrap();
        assert_eq!(toml::from_str::<JobSpec>(&encoded).unwrap(), job);
        // Absent policy stays absent on the wire.
        let plain: JobSpec = toml::from_str("image='fetcher:v1'").unwrap();
        let plain_encoded = toml::to_string(&plain).unwrap();
        assert!(!plain_encoded.contains("egress") && !plain_encoded.contains("firewall"));
    }

    #[test]
    fn pids_limit_is_part_of_the_job_and_absent_stays_absent_on_the_wire() {
        let job: JobSpec = toml::from_str("image='fixture:v1'\npids = 512").unwrap();
        assert_eq!(job.pids.map(PidsLimit::get), Some(512));
        // It survives both encodings a job spec travels in: TOML into Raft
        // desired state and JSON through the API.
        let encoded = toml::to_string(&job).unwrap();
        assert!(encoded.contains("pids = 512"), "{encoded}");
        assert_eq!(toml::from_str::<JobSpec>(&encoded).unwrap(), job);
        let json = serde_json::to_string(&job).unwrap();
        assert!(json.contains(r#""pids":512"#), "{json}");
        assert_eq!(serde_json::from_str::<JobSpec>(&json).unwrap(), job);
        let plain: JobSpec = toml::from_str("image='fixture:v1'").unwrap();
        assert_eq!(plain.pids, None);
        assert!(!toml::to_string(&plain).unwrap().contains("pids"));
        assert!(!serde_json::to_string(&plain).unwrap().contains("pids"));
    }

    #[test]
    fn pids_limit_accepts_one_to_the_maximum_and_refuses_everything_else() {
        for accepted in [1, 256, PidsLimit::MAX] {
            let job: JobSpec =
                toml::from_str(&format!("image='fixture:v1'\npids = {accepted}")).unwrap();
            assert_eq!(job.pids.map(PidsLimit::get), Some(accepted));
        }
        for refused in ["0", "32769", "4294967295", "-1", "'512'", "1.5"] {
            assert!(
                toml::from_str::<JobSpec>(&format!("image='fixture:v1'\npids = {refused}"))
                    .is_err(),
                "pids = {refused} was accepted"
            );
        }
        // The JSON API refuses the same values.
        assert!(serde_json::from_str::<JobSpec>(r#"{"image":"fixture:v1","pids":0}"#).is_err());
        assert!(PidsLimit::try_from(0).is_err());
        assert!(PidsLimit::try_from(PidsLimit::MAX + 1).is_err());
        // A whole config names the bounds in its refusal.
        let error = crate::config::Config::parse("[job.worker]\nimage='fixture:v1'\npids = 0")
            .unwrap_err()
            .to_string();
        assert!(error.contains("between 1 and 32768"), "{error}");
    }

    #[test]
    fn parse_job_with_resources() {
        let toml_str = r#"
            image = "myapp:v1"
            memory = "512Mi"
            cpu = "500m"
        "#;
        let j: JobSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(j.memory.unwrap().request, 512 * 1024 * 1024);
        assert_eq!(j.cpu.unwrap().request, 500);
    }
}

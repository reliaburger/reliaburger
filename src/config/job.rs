/// Job specification — a run-to-completion task.
///
/// Jobs can run inside a container (with `image`) or as a host process
/// (`exec`/`script`, Phase 8). They support cron scheduling and
/// dependency ordering via `run_before`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{EnvValue, ResourceRange};

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
}

impl JobSpec {
    /// Refuse fields that contradict the explicitly selected execution backend.
    /// Whether this job runs a host command rather than a container.
    ///
    /// For a validated spec that is exactly `runtime = "process"`. An
    /// unvalidated one that names a host command anywhere counts as host too,
    /// so routing and the `host-exec` permission check never treat a host
    /// command as a container, whether or not validation ran first.
    pub fn is_host(&self) -> bool {
        self.runtime == JobRuntime::Process || self.exec.is_some() || self.script.is_some()
    }

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

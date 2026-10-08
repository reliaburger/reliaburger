/// Job specification — a run-to-completion task.
///
/// Jobs can run inside a container (with `image`) or as a host process
/// (`exec`/`script`, Phase 8). They support cron scheduling and
/// dependency ordering via `run_before`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{EnvValue, ResourceRange};

/// Whether each attempt receives a new OCI container or a command slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerIsolation {
    /// A new owned container generation for each attempt.
    #[default]
    FreshContainer,
    /// A separate command process inside a compatible reusable container.
    ReusableContainer,
}

impl ContainerIsolation {
    fn is_fresh(&self) -> bool {
        *self == Self::FreshContainer
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// Container isolation; reuse is explicit and image-only.
    #[serde(default, skip_serializing_if = "ContainerIsolation::is_fresh")]
    pub isolation: ContainerIsolation,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_reuse_is_explicit_and_fresh_is_the_default() {
        let fresh: JobSpec = toml::from_str("image='fixture:v1'").unwrap();
        assert_eq!(fresh.isolation, ContainerIsolation::FreshContainer);
        let reused: JobSpec = toml::from_str(
            "image='fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nisolation='reusable-container'",
        ).unwrap();
        assert_eq!(reused.isolation, ContainerIsolation::ReusableContainer);
        let encoded = toml::to_string(&reused).unwrap();
        assert!(encoded.contains("isolation = \"reusable-container\""));
        assert!(!toml::to_string(&fresh).unwrap().contains("isolation"));
        assert!(
            toml::from_str::<JobSpec>("image='fixture:v1'\nisolation='resident-worker'").is_err()
        );
    }

    #[test]
    fn container_reuse_refuses_host_execution_before_admission() {
        let source = "[job.worker]\nexec='/bin/true'\nisolation='reusable-container'";
        let error = crate::config::Config::parse(source)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("reusable-container"));
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

/// Job specification — a run-to-completion task.
///
/// Jobs can run inside a container (with `image`) or as a host process
/// (`exec`/`script`, Phase 8). They support cron scheduling and
/// dependency ordering via `run_before`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{EnvValue, ResourceRange};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ResourceRange>,
    /// CPU request-limit range.
    #[serde(skip_serializing_if = "Option::is_none")]
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

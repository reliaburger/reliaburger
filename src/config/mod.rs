/// Configuration parsing for Reliaburger.
///
/// Two independent configuration files:
/// - **Workload config** (`Config`): defines apps, jobs, namespaces, permissions
/// - **Node config** (`NodeConfig`): defines per-node settings like storage paths,
///   resource reservations, and cluster membership
///
/// Both parse from TOML and validate in a separate pass.
pub mod app;
pub mod error;
pub mod job;
pub mod namespace;
pub mod node;
pub mod permission;
pub mod types;
mod validate;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use app::AppSpec;
pub use error::ConfigError;
pub use job::JobSpec;
pub use namespace::NamespaceSpec;
pub use node::NodeConfig;
pub use permission::PermissionSpec;
pub use types::{ConfigFileSpec, EnvValue, Replicas, ResourceRange, VolumeSpec};

/// Top-level workload configuration.
///
/// Parsed from a TOML file containing `[app.*]`, `[job.*]`,
/// `[namespace.*]`, and `[permission.*]` tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// App definitions keyed by name.
    #[serde(default)]
    pub app: BTreeMap<String, AppSpec>,
    /// Job definitions keyed by name.
    #[serde(default)]
    pub job: BTreeMap<String, JobSpec>,
    /// Namespace definitions keyed by name.
    #[serde(default)]
    pub namespace: BTreeMap<String, NamespaceSpec>,
    /// Permission definitions keyed by name.
    #[serde(default)]
    pub permission: BTreeMap<String, PermissionSpec>,
}

impl Config {
    /// Parse workload configuration from a TOML string.
    pub fn parse(toml: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(toml)?)
    }

    /// Parse workload configuration from a file.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_empty_file() {
        let config = Config::parse("").unwrap();
        assert!(config.app.is_empty());
        assert!(config.job.is_empty());
        assert!(config.namespace.is_empty());
        assert!(config.permission.is_empty());
    }

    #[test]
    fn parse_config_only_apps() {
        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3
            port = 8080

            [app.api]
            image = "api:v2"
        "#,
        )
        .unwrap();
        assert_eq!(config.app.len(), 2);
        assert!(config.app.contains_key("web"));
        assert!(config.app.contains_key("api"));
        assert!(config.job.is_empty());
    }

    #[test]
    fn parse_config_with_apps_and_jobs() {
        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"

            [job.migrate]
            image = "myapp:v1"
            command = ["npm", "run", "migrate"]
            run_before = ["app.web"]
        "#,
        )
        .unwrap();
        assert_eq!(config.app.len(), 1);
        assert_eq!(config.job.len(), 1);
        assert_eq!(config.job["migrate"].run_before, vec!["app.web"]);
    }

    #[test]
    fn parse_config_with_all_resource_types() {
        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3

            [job.cleanup]
            image = "cleanup:latest"
            schedule = "0 3 * * *"

            [namespace.backend]
            cpu = "8000m"
            memory = "16Gi"
            max_apps = 50

            [permission.deployer]
            actions = ["deploy", "scale"]
            apps = ["web"]
        "#,
        )
        .unwrap();
        assert_eq!(config.app.len(), 1);
        assert_eq!(config.job.len(), 1);
        assert_eq!(config.namespace.len(), 1);
        assert_eq!(config.permission.len(), 1);

        assert_eq!(config.namespace["backend"].max_apps, Some(50));
        assert_eq!(
            config.permission["deployer"].actions,
            vec!["deploy", "scale"]
        );
    }

    #[test]
    fn parse_config_from_whitepaper_example() {
        // The complete example from the whitepaper (updated for health object)
        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1.4.2"
            replicas = 3
            port = 8080
            memory = "128Mi-512Mi"
            cpu = "100m-500m"

            [app.web.health]
            path = "/healthz"

            [app.web.ingress]
            host = "myapp.com"

            [app.web.placement]
            required = ["region=us-east"]
            preferred = ["ssd=true"]

            [app.web.deploy]
            strategy = "rolling"
            max_unavailable = 1
            auto_rollback = true

            [app.web.env]
            DATABASE_URL = "ENC[AGE:YWdlLWVuY3J5cHRpb24...]"
            NODE_ENV = "production"

            [app.redis]
            image = "redis:7-alpine"
            port = 6379
            [[app.redis.volumes]]
            path = "/data"
            size = "10Gi"

            [job.db-migrate]
            image = "myapp:v1.4.2"
            command = ["npm", "run", "migrate"]
            run_before = ["app.api"]

            [job.cleanup]
            image = "cleanup:latest"
            schedule = "0 3 * * *"

            [namespace.team-backend]
            cpu = "8000m"
            memory = "16Gi"
            gpu = 2
            max_apps = 50
            max_replicas = 200

            [permission.deployer]
            actions = ["deploy", "scale", "logs", "metrics"]
            apps = ["web", "api"]
        "#,
        )
        .unwrap();

        assert_eq!(config.app.len(), 2);
        assert_eq!(config.job.len(), 2);
        assert_eq!(config.namespace.len(), 1);
        assert_eq!(config.permission.len(), 1);

        // Verify specific fields
        let web = &config.app["web"];
        assert_eq!(web.replicas, Replicas::Fixed(3));
        assert!(web.health.is_some());
        assert_eq!(web.health.as_ref().unwrap().path, "/healthz");
        assert!(web.env.get("DATABASE_URL").unwrap().is_encrypted());
        assert!(!web.env.get("NODE_ENV").unwrap().is_encrypted());

        let redis = &config.app["redis"];
        assert_eq!(redis.volumes.len(), 1);
        assert_eq!(redis.volumes[0].size.as_deref(), Some("10Gi"));

        let ns = &config.namespace["team-backend"];
        assert_eq!(ns.gpu, Some(2));
        assert_eq!(ns.max_replicas, Some(200));
    }
}

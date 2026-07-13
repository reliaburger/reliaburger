use super::Config;
/// Validation for parsed configuration.
///
/// Validation runs as a separate pass after TOML parsing succeeds.
/// This separation gives TOML errors line numbers and domain errors
/// meaningful context.
use super::error::ConfigError;
use super::node::NodeConfig;
use super::types::parse_resource_value;

impl Config {
    /// Validate the parsed configuration.
    ///
    /// Returns the first error found. Call after `from_str` or `from_file`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, app) in &self.app {
            validate_app(name, app)?;
        }
        for (name, job) in &self.job {
            validate_job(name, job)?;
        }
        for (name, ns) in &self.namespace {
            validate_namespace(name, ns)?;
        }
        // Permissions and builds may reference namespaces declared in the
        // same file. Apply passes the already-committed desired-state
        // namespaces through `validate_against`; a bare `validate` only
        // knows about namespaces in this config.
        let declared: Vec<String> = self.namespace.keys().cloned().collect();
        for (name, perm) in &self.permission {
            validate_permission(name, perm, &declared)?;
        }
        for (name, build) in &self.build {
            validate_build(name, build, &declared)?;
        }
        Ok(())
    }

    /// Validate this config, treating `known_namespaces` (from committed
    /// desired state) as also declared. Permissions and builds may target
    /// a namespace created by an earlier apply, not just one in this file.
    pub fn validate_against(&self, known_namespaces: &[String]) -> Result<(), ConfigError> {
        for (name, app) in &self.app {
            validate_app(name, app)?;
        }
        for (name, job) in &self.job {
            validate_job(name, job)?;
        }
        for (name, ns) in &self.namespace {
            validate_namespace(name, ns)?;
        }
        let mut declared: Vec<String> = self.namespace.keys().cloned().collect();
        declared.extend(known_namespaces.iter().cloned());
        for (name, perm) in &self.permission {
            validate_permission(name, perm, &declared)?;
        }
        for (name, build) in &self.build {
            validate_build(name, build, &declared)?;
        }
        Ok(())
    }
}

/// Actions a `[[permission]]` may grant. Mirrors `permission.rs`'s doc.
const KNOWN_PERMISSION_ACTIONS: &[&str] = &[
    "deploy",
    "scale",
    "logs",
    "metrics",
    "exec",
    "host-exec",
    "admin",
    "secret-read",
    "secret-write",
];

fn validate_namespace(name: &str, ns: &super::namespace::NamespaceSpec) -> Result<(), ConfigError> {
    // Resource budgets must parse as non-negative resource values. A
    // negative or overflowing CPU/memory budget is a typo, not a quota.
    if let Some(cpu) = &ns.cpu {
        parse_resource_value(cpu).map_err(|_| ConfigError::Validation {
            field: "cpu".to_string(),
            context: format!("namespace {name:?}"),
            reason: format!("invalid resource value {cpu:?}"),
        })?;
    }
    if let Some(memory) = &ns.memory {
        parse_resource_value(memory).map_err(|_| ConfigError::Validation {
            field: "memory".to_string(),
            context: format!("namespace {name:?}"),
            reason: format!("invalid resource value {memory:?}"),
        })?;
    }
    // A zero cap means "admit nothing", which is never what an operator
    // wants and almost always a mistake. Omit the field for "no limit".
    for (field, value) in [
        ("max_apps", ns.max_apps),
        ("max_replicas", ns.max_replicas),
        ("gpu", ns.gpu),
    ] {
        if value == Some(0) {
            return Err(ConfigError::Validation {
                field: field.to_string(),
                context: format!("namespace {name:?}"),
                reason: format!("{field} must be positive; omit it for no limit"),
            });
        }
    }
    Ok(())
}

fn validate_permission(
    name: &str,
    perm: &super::permission::PermissionSpec,
    known_namespaces: &[String],
) -> Result<(), ConfigError> {
    for action in &perm.actions {
        if !KNOWN_PERMISSION_ACTIONS.contains(&action.as_str()) {
            return Err(ConfigError::Validation {
                field: "actions".to_string(),
                context: format!("permission {name:?}"),
                reason: format!(
                    "unknown action {action:?}; valid actions are {}",
                    KNOWN_PERMISSION_ACTIONS.join(", ")
                ),
            });
        }
    }
    // A permission scoped to a namespace that doesn't exist grants nothing
    // and hides a typo. `default` always exists implicitly.
    if let Some(namespaces) = &perm.namespaces {
        for ns in namespaces {
            if ns != "default" && !known_namespaces.iter().any(|n| n == ns) {
                return Err(ConfigError::Validation {
                    field: "namespaces".to_string(),
                    context: format!("permission {name:?}"),
                    reason: format!("references unknown namespace {ns:?}"),
                });
            }
        }
    }
    Ok(())
}

fn validate_build(
    name: &str,
    build: &super::build::BuildSpec,
    known_namespaces: &[String],
) -> Result<(), ConfigError> {
    // Destination syntax and context existence.
    super::build::validate_build_namespace(name, build)?;
    // A build's namespace must exist. `default` is implicit.
    if let Some(ns) = &build.namespace
        && ns != "default"
        && !known_namespaces.iter().any(|n| n == ns)
    {
        return Err(ConfigError::Validation {
            field: "namespace".to_string(),
            context: format!("build {name:?}"),
            reason: format!("references unknown namespace {ns:?}"),
        });
    }
    Ok(())
}

fn validate_app(name: &str, app: &super::app::AppSpec) -> Result<(), ConfigError> {
    // An app needs at least one of: image, command, exec, or script
    if app.image.is_none() && app.command.is_empty() && app.exec.is_none() && app.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
        });
    }

    // exec and image are mutually exclusive
    if app.exec.is_some() && app.image.is_some() {
        return Err(ConfigError::Validation {
            field: "exec".to_string(),
            context: format!("app {name:?}"),
            reason: "exec and image are mutually exclusive".to_string(),
        });
    }

    // script and image are mutually exclusive
    if app.script.is_some() && app.image.is_some() {
        return Err(ConfigError::Validation {
            field: "script".to_string(),
            context: format!("app {name:?}"),
            reason: "script and image are mutually exclusive".to_string(),
        });
    }

    // exec and script are mutually exclusive
    if app.exec.is_some() && app.script.is_some() {
        return Err(ConfigError::Validation {
            field: "exec/script".to_string(),
            context: format!("app {name:?}"),
            reason: "exec and script are mutually exclusive".to_string(),
        });
    }

    // Port range
    if let Some(port) = app.port
        && port == 0
    {
        return Err(ConfigError::InvalidPort {
            name: name.to_string(),
            port,
        });
    }

    // Config files: exactly one of content/source
    for cf in &app.config_file {
        match (&cf.content, &cf.source) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(ConfigError::InvalidConfigFile {
                    name: name.to_string(),
                });
            }
            _ => {}
        }
    }

    // Volumes: paths must be absolute
    for vol in &app.volumes {
        if !vol.path.is_absolute() {
            return Err(ConfigError::InvalidVolume {
                name: name.to_string(),
                reason: format!("mount path {:?} must be absolute", vol.path.display()),
            });
        }
        if let Some(ref source) = vol.source
            && !source.is_absolute()
        {
            return Err(ConfigError::InvalidVolume {
                name: name.to_string(),
                reason: format!("source path {:?} must be absolute", source.display()),
            });
        }
    }

    // Autoscale block: bounds, windows and threshold are validated up front
    // so a bad `[autoscale]` fails the deploy instead of silently clamping
    // at runtime (DEP8).
    if let Some(autoscale) = &app.autoscale
        && let Err(e) = crate::meat::autoscaler::AutoscaleConfig::from_spec(autoscale)
    {
        return Err(ConfigError::Validation {
            field: "autoscale".to_string(),
            context: format!("app {name:?}"),
            reason: e.to_string(),
        });
    }

    Ok(())
}

fn validate_job(name: &str, job: &super::job::JobSpec) -> Result<(), ConfigError> {
    // Must have at least one of image, exec, or script
    if job.image.is_none() && job.exec.is_none() && job.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
        });
    }

    // exec and image are mutually exclusive
    if job.exec.is_some() && job.image.is_some() {
        return Err(ConfigError::Validation {
            field: "exec".to_string(),
            context: format!("job {name:?}"),
            reason: "exec and image are mutually exclusive".to_string(),
        });
    }

    // script and image are mutually exclusive
    if job.script.is_some() && job.image.is_some() {
        return Err(ConfigError::Validation {
            field: "script".to_string(),
            context: format!("job {name:?}"),
            reason: "script and image are mutually exclusive".to_string(),
        });
    }

    // exec and script are mutually exclusive
    if job.exec.is_some() && job.script.is_some() {
        return Err(ConfigError::Validation {
            field: "exec/script".to_string(),
            context: format!("job {name:?}"),
            reason: "exec and script are mutually exclusive".to_string(),
        });
    }

    Ok(())
}

impl NodeConfig {
    /// Validate the parsed node configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Storage paths must be absolute
        let paths = [
            ("storage.data", &self.storage.data),
            ("storage.images", &self.storage.images),
            ("storage.logs", &self.storage.logs),
            ("storage.metrics", &self.storage.metrics),
            ("storage.volumes", &self.storage.volumes),
        ];
        for (field, path) in &paths {
            if !path.is_absolute() {
                return Err(ConfigError::NonAbsolutePath {
                    field: (*field).to_string(),
                    path: (*path).clone(),
                });
            }
        }

        // Port range validation
        if self.network.port_range.start >= self.network.port_range.end {
            return Err(ConfigError::InvalidPortRange {
                value: format!(
                    "{}-{}",
                    self.network.port_range.start, self.network.port_range.end
                ),
                reason: "start must be less than end".to_string(),
            });
        }

        // Upgrade keys must parse; retention must keep at least one version
        let upgrade_validation = |field: &str, reason: String| ConfigError::Validation {
            field: field.to_string(),
            context: "node config".to_string(),
            reason,
        };
        if let Some(key) = &self.upgrades.external_signing_key {
            crate::upgrade::signing::parse_public_key(key)
                .map_err(|e| upgrade_validation("upgrades.external_signing_key", e.to_string()))?;
        }
        for key in self.upgrades.release_keys_override.iter().flatten() {
            crate::upgrade::signing::parse_public_key(key)
                .map_err(|e| upgrade_validation("upgrades.release_keys_override", e.to_string()))?;
        }
        if self.upgrades.retain_versions == 0 {
            return Err(upgrade_validation(
                "upgrades.retain_versions",
                "must retain at least one version".to_string(),
            ));
        }
        if let Some(dir) = &self.upgrades.binary_dir
            && !dir.is_absolute()
        {
            return Err(ConfigError::NonAbsolutePath {
                field: "upgrades.binary_dir".to_string(),
                path: dir.clone(),
            });
        }

        // Reserved resources must parse
        parse_resource_value(&self.resources.reserved_cpu).map_err(|_| {
            ConfigError::Validation {
                field: "resources.reserved_cpu".to_string(),
                context: "node config".to_string(),
                reason: format!("invalid resource value {:?}", self.resources.reserved_cpu),
            }
        })?;
        parse_resource_value(&self.resources.reserved_memory).map_err(|_| {
            ConfigError::Validation {
                field: "resources.reserved_memory".to_string(),
                context: "node config".to_string(),
                reason: format!(
                    "invalid resource value {:?}",
                    self.resources.reserved_memory
                ),
            }
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::AppSpec;
    use crate::config::types::ConfigFileSpec;
    use std::path::PathBuf;

    fn config_with_app(name: &str, app: AppSpec) -> Config {
        let mut config = Config::default();
        config.app.insert(name.to_string(), app);
        config
    }

    fn minimal_app() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    #[test]
    fn validate_app_missing_image_rejected() {
        let app: AppSpec = toml::from_str("replicas = 1").unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::MissingImage { ref name } if name == "test"),
            "expected MissingImage, got {err:?}"
        );
    }

    #[test]
    fn validate_config_file_both_content_and_source_rejected() {
        let mut app = minimal_app();
        app.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: Some("data".to_string()),
            source: Some("configs/app.conf".to_string()),
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidConfigFile { .. })
        ));
    }

    #[test]
    fn validate_config_file_neither_content_nor_source_rejected() {
        let mut app = minimal_app();
        app.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: None,
            source: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidConfigFile { .. })
        ));
    }

    #[test]
    fn validate_port_zero_rejected() {
        let mut app = minimal_app();
        app.port = Some(0);
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidPort { .. })
        ));
    }

    #[test]
    fn validate_valid_app_passes() {
        let config = config_with_app("test", minimal_app());
        config.validate().unwrap();
    }

    #[test]
    fn validate_autoscale_min_greater_than_max_rejected() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "web:v1"
            [autoscale]
            metric = "cpu"
            target = "70%"
            min = 10
            max = 3
        "#,
        )
        .unwrap();
        let config = config_with_app("web", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "autoscale"),
            "min>max autoscale must be rejected at validation: {err:?}"
        );
    }

    #[test]
    fn validate_node_config_non_absolute_path_rejected() {
        let mut nc = NodeConfig::default();
        nc.storage.data = PathBuf::from("relative/path");
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::NonAbsolutePath { .. })
        ));
    }

    #[test]
    fn validate_node_config_invalid_port_range_rejected() {
        let mut nc = NodeConfig::default();
        nc.network.port_range.start = 60000;
        nc.network.port_range.end = 10000;
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::InvalidPortRange { .. })
        ));
    }

    #[test]
    fn validate_valid_node_config_passes() {
        let nc = NodeConfig::default();
        nc.validate().unwrap();
    }

    #[test]
    fn upgrades_section_defaults() {
        let nc = NodeConfig::default();
        assert_eq!(nc.upgrades.retain_versions, 3);
        assert_eq!(nc.upgrades.max_boot_attempts, 2);
        assert!(nc.upgrades.external_signing_key.is_none());
        assert!(nc.upgrades.release_keys_override.is_none());
        nc.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_external_signing_key() {
        let mut nc = NodeConfig::default();
        nc.upgrades.external_signing_key = Some("not-a-key".to_string());
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn release_keys_override_requires_valid_keys() {
        let mut nc = NodeConfig::default();
        nc.upgrades.release_keys_override = Some(vec!["ed25519:c2hvcnQ=".to_string()]); // valid base64, wrong length
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn upgrades_retain_versions_zero_rejected() {
        let mut nc = NodeConfig::default();
        nc.upgrades.retain_versions = 0;
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn upgrades_relative_binary_dir_rejected() {
        let mut nc = NodeConfig::default();
        nc.upgrades.binary_dir = Some(PathBuf::from("relative/bin"));
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::NonAbsolutePath { .. })
        ));
    }

    #[test]
    fn validate_volume_relative_mount_path_rejected() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("relative/data"),
            source: None,
            size: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn validate_volume_relative_source_path_rejected() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("relative/host/path")),
            size: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn validate_volume_absolute_paths_passes() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("/host/data")),
            size: None,
        });
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    // -----------------------------------------------------------------------
    // Process workload validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_exec_and_image_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            exec = "/usr/bin/python3"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")),
            "expected mutual exclusivity error, got {err:?}"
        );
    }

    #[test]
    fn validate_script_and_image_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            script = "echo hello"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")),
            "expected mutual exclusivity error, got {err:?}"
        );
    }

    #[test]
    fn validate_exec_and_script_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            exec = "/usr/bin/python3"
            script = "echo hello"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "exec/script"),
            "expected exec/script error, got {err:?}"
        );
    }

    #[test]
    fn validate_exec_only_passes() {
        let app: AppSpec = toml::from_str(r#"exec = "/usr/bin/python3""#).unwrap();
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    #[test]
    fn validate_script_only_passes() {
        let app: AppSpec = toml::from_str(r#"script = "echo hello""#).unwrap();
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    #[test]
    fn validate_job_exec_and_image_mutually_exclusive() {
        let job: crate::config::job::JobSpec = toml::from_str(
            r#"
            image = "test:v1"
            exec = "/usr/bin/python3"
        "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.job.insert("test-job".to_string(), job);
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")
        ));
    }

    #[test]
    fn validate_job_exec_only_passes() {
        let job: crate::config::job::JobSpec =
            toml::from_str(r#"exec = "/usr/bin/python3""#).unwrap();
        let mut config = Config::default();
        config.job.insert("test-job".to_string(), job);
        config.validate().unwrap();
    }

    // -----------------------------------------------------------------------
    // Namespace / permission / build validation (12b.2 T6)
    // -----------------------------------------------------------------------

    #[test]
    fn validate_namespace_negative_quota_rejected() {
        let config = Config::parse(
            r#"
            [namespace.team]
            cpu = "-5"
        "#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "cpu"),
            "negative cpu budget must be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_namespace_zero_max_apps_rejected() {
        let config = Config::parse(
            r#"
            [namespace.team]
            max_apps = 0
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_namespace_valid_quota_passes() {
        let config = Config::parse(
            r#"
            [namespace.team]
            cpu = "8000m"
            memory = "16Gi"
            gpu = 2
            max_apps = 50
            max_replicas = 200
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_permission_unknown_action_rejected() {
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy", "teleport"]
        "#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "actions"),
            "unknown permission action must be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_permission_unknown_namespace_rejected() {
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy"]
            namespaces = ["ghost"]
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_permission_namespace_in_same_config_passes() {
        let config = Config::parse(
            r#"
            [namespace.prod]
            cpu = "8000m"

            [permission.p]
            actions = ["deploy", "scale"]
            namespaces = ["prod"]
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_permission_namespace_from_desired_state_passes() {
        // `prod` isn't in this file but was created by an earlier apply.
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy"]
            namespaces = ["prod"]
        "#,
        )
        .unwrap();
        assert!(config.validate().is_err(), "bare validate can't see prod");
        config
            .validate_against(&["prod".to_string()])
            .expect("validate_against sees the committed namespace");
    }

    #[test]
    fn validate_build_unknown_namespace_rejected() {
        let config = Config::parse(
            r#"
            [build.img]
            context = "."
            destination = "pickle://img:v1"
            namespace = "ghost"
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_build_existing_namespace_passes() {
        let config = Config::parse(
            r#"
            [namespace.prod]
            cpu = "8000m"

            [build.img]
            context = "."
            destination = "pickle://img:v1"
            namespace = "prod"
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }
}

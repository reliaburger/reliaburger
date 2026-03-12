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
        Ok(())
    }
}

fn validate_app(name: &str, app: &super::app::AppSpec) -> Result<(), ConfigError> {
    // An app needs at least one of: image, command, exec, or script
    if app.image.is_none() && app.command.is_empty() && app.exec.is_none() && app.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
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

    Ok(())
}

fn validate_job(name: &str, job: &super::job::JobSpec) -> Result<(), ConfigError> {
    // Must have at least one of image, exec, or script
    if job.image.is_none() && job.exec.is_none() && job.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
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
}

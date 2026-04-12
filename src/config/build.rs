//! Build job configuration.
//!
//! A build job produces an OCI image from a Dockerfile and pushes it
//! to the local Pickle registry. The `pickle://` destination syntax
//! restricts pushes to namespace-scoped repositories.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::error::ConfigError;

/// Specification for an in-cluster image build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// Path to the build context (directory containing the Dockerfile).
    pub context: PathBuf,

    /// Path to the Dockerfile, relative to context. Defaults to "Dockerfile".
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,

    /// Destination in `pickle://name:tag` format.
    /// The image is pushed to the local Pickle registry.
    pub destination: String,

    /// Build arguments (key=value pairs passed to the builder).
    #[serde(default)]
    pub args: BTreeMap<String, String>,

    /// Namespace this build belongs to. Restricts push to
    /// namespace-scoped repositories.
    pub namespace: Option<String>,
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

/// Parsed `pickle://` destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickleDestination {
    /// Image name (e.g. "myapp").
    pub name: String,
    /// Image tag (e.g. "v1.2.3"). Defaults to "latest".
    pub tag: String,
}

/// Parse a `pickle://name:tag` destination string.
///
/// Returns the image name and tag. If no tag is specified,
/// defaults to "latest".
pub fn parse_pickle_destination(dest: &str) -> Result<PickleDestination, ConfigError> {
    let stripped = dest
        .strip_prefix("pickle://")
        .ok_or_else(|| ConfigError::Validation {
            field: "destination".to_string(),
            context: "build spec".to_string(),
            reason: format!("destination must start with pickle://, got {dest:?}"),
        })?;

    if stripped.is_empty() {
        return Err(ConfigError::Validation {
            field: "destination".to_string(),
            context: "build spec".to_string(),
            reason: "destination name cannot be empty".to_string(),
        });
    }

    let (name, tag) = if let Some((n, t)) = stripped.split_once(':') {
        (n.to_string(), t.to_string())
    } else {
        (stripped.to_string(), "latest".to_string())
    };

    if name.is_empty() {
        return Err(ConfigError::Validation {
            field: "destination".to_string(),
            context: "build spec".to_string(),
            reason: "image name cannot be empty".to_string(),
        });
    }

    if tag.is_empty() {
        return Err(ConfigError::Validation {
            field: "destination".to_string(),
            context: "build spec".to_string(),
            reason: "image tag cannot be empty (omit colon for :latest)".to_string(),
        });
    }

    Ok(PickleDestination { name, tag })
}

/// Validate that a build's destination namespace matches its declared namespace.
///
/// Prevents a build in namespace A from pushing to namespace B's scope.
pub fn validate_build_namespace(build_name: &str, spec: &BuildSpec) -> Result<(), ConfigError> {
    // Destination must use pickle:// protocol
    parse_pickle_destination(&spec.destination)?;

    // Context path must exist (or be relative — we check absolute paths only)
    if spec.context.is_absolute() && !spec.context.exists() {
        return Err(ConfigError::Validation {
            field: "context".to_string(),
            context: format!("build {build_name:?}"),
            reason: format!("context path {:?} does not exist", spec.context),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pickle_destination_with_tag() {
        let d = parse_pickle_destination("pickle://myapp:v1.2.3").unwrap();
        assert_eq!(d.name, "myapp");
        assert_eq!(d.tag, "v1.2.3");
    }

    #[test]
    fn parse_pickle_destination_default_tag() {
        let d = parse_pickle_destination("pickle://myapp").unwrap();
        assert_eq!(d.name, "myapp");
        assert_eq!(d.tag, "latest");
    }

    #[test]
    fn parse_pickle_destination_missing_prefix() {
        let result = parse_pickle_destination("docker://myapp:v1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pickle://"));
    }

    #[test]
    fn parse_pickle_destination_empty_name() {
        let result = parse_pickle_destination("pickle://");
        assert!(result.is_err());
    }

    #[test]
    fn parse_pickle_destination_empty_tag() {
        let result = parse_pickle_destination("pickle://myapp:");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("tag"));
    }

    #[test]
    fn build_spec_parses_from_toml() {
        let toml_str = r#"
            context = "./src"
            destination = "pickle://myapp:v1"
            namespace = "production"

            [args]
            RUST_VERSION = "1.78"
            FEATURES = "ebpf"
        "#;
        let spec: BuildSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(spec.context, PathBuf::from("./src"));
        assert_eq!(spec.dockerfile, "Dockerfile");
        assert_eq!(spec.destination, "pickle://myapp:v1");
        assert_eq!(spec.namespace.as_deref(), Some("production"));
        assert_eq!(spec.args.len(), 2);
        assert_eq!(spec.args["RUST_VERSION"], "1.78");
    }

    #[test]
    fn build_spec_parses_minimal() {
        let toml_str = r#"
            context = "."
            destination = "pickle://app"
        "#;
        let spec: BuildSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(spec.dockerfile, "Dockerfile");
        assert!(spec.args.is_empty());
        assert!(spec.namespace.is_none());
    }

    #[test]
    fn build_spec_custom_dockerfile() {
        let toml_str = r#"
            context = "."
            dockerfile = "Dockerfile.prod"
            destination = "pickle://app:prod"
        "#;
        let spec: BuildSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(spec.dockerfile, "Dockerfile.prod");
    }

    #[test]
    fn validate_build_rejects_non_pickle_destination() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "docker://myapp:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
        };
        assert!(validate_build_namespace("test-build", &spec).is_err());
    }

    #[test]
    fn validate_build_accepts_pickle_destination() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://myapp:v1".into(),
            args: BTreeMap::new(),
            namespace: Some("default".into()),
        };
        assert!(validate_build_namespace("test-build", &spec).is_ok());
    }
}

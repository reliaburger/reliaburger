//! Build job configuration.
//!
//! A build job produces an OCI image from a Dockerfile and pushes it
//! to the local Pickle registry, at a `pickle://` destination. A build
//! that declares a `namespace` must push into that namespace's
//! repositories (`pickle://<namespace>/<name>:<tag>`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::error::ConfigError;

/// Specification for an in-cluster image build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    /// Path to the build context (directory containing the Dockerfile).
    pub context: PathBuf,

    /// Path to the Dockerfile, relative to context. Defaults to "Dockerfile".
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,

    /// Destination in `pickle://name:tag` or `pickle://namespace/name:tag`
    /// format. The image is pushed to the local Pickle registry.
    pub destination: String,

    /// Build arguments (key=value pairs passed to the builder).
    #[serde(default)]
    pub args: BTreeMap<String, String>,

    /// Namespace this build belongs to. When set, the destination must
    /// be a repository in this namespace (`pickle://<namespace>/<name>`).
    pub namespace: Option<String>,

    /// Target platform(s) for the build.
    ///
    /// Defaults to `["linux/amd64", "linux/arm64"]` so images work on
    /// mixed-architecture clusters. Buildah produces a manifest list
    /// (OCI index) when multiple platforms are specified.
    #[serde(default = "default_platforms")]
    pub platform: Vec<String>,
}

fn default_platforms() -> Vec<String> {
    vec!["linux/amd64".to_string(), "linux/arm64".to_string()]
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

/// Validate a build's destination: `pickle://` syntax, a context that
/// exists, and a destination inside the build's declared namespace.
///
/// A build in namespace A can't push to namespace B's repositories, nor to
/// a bare name that belongs to no namespace ([`check_destination_namespace`]).
pub fn validate_build_namespace(build_name: &str, spec: &BuildSpec) -> Result<(), ConfigError> {
    check_destination_namespace(spec)?;

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

/// Check that a build which declares a `namespace` pushes into it.
///
/// The destination's repository must be named `<namespace>/<name>` by the
/// same rule the registry uses to place a repository in a namespace
/// ([`crate::pickle::registry_auth::repository_namespace`]). A bare name
/// (`pickle://api:v1`) belongs to no namespace, so it's refused rather than
/// quietly scoped: the push would otherwise land in a repository every
/// namespace can pull. A build with no `namespace` may push anywhere its
/// caller's token allows, which the API checks separately.
pub fn check_destination_namespace(spec: &BuildSpec) -> Result<(), ConfigError> {
    let destination = parse_pickle_destination(&spec.destination)?;
    let Some(namespace) = &spec.namespace else {
        return Ok(());
    };
    let reason = match crate::pickle::registry_auth::repository_namespace(&destination.name) {
        Some((dest_namespace, _)) if dest_namespace == namespace => return Ok(()),
        Some((dest_namespace, _)) => format!(
            "build is in namespace {namespace:?} but destination {:?} pushes to namespace \
             {dest_namespace:?}",
            spec.destination
        ),
        None if destination.name.contains('/') => format!(
            "build is in namespace {namespace:?} but destination {:?} is not a repository \
             in any namespace",
            spec.destination
        ),
        None => format!(
            "build is in namespace {namespace:?} but destination {:?} names no namespace; \
             use pickle://{namespace}/{}:{}",
            spec.destination, destination.name, destination.tag
        ),
    };
    Err(ConfigError::Validation {
        field: "destination".to_string(),
        context: "build spec".to_string(),
        reason,
    })
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
            platform: vec![],
        };
        assert!(validate_build_namespace("test-build", &spec).is_err());
    }

    #[test]
    fn validate_build_accepts_pickle_destination() {
        let spec = spec("pickle://default/myapp:v1", Some("default"));
        assert!(validate_build_namespace("test-build", &spec).is_ok());
    }

    fn spec(destination: &str, namespace: Option<&str>) -> BuildSpec {
        BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: destination.into(),
            args: BTreeMap::new(),
            namespace: namespace.map(str::to_string),
            platform: vec![],
        }
    }

    #[test]
    fn namespaced_build_with_a_bare_destination_is_refused() {
        let error = validate_build_namespace("api", &spec("pickle://api:v1", Some("production")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("production"), "{error}");
        assert!(error.contains("pickle://production/api:v1"), "{error}");
    }

    #[test]
    fn default_namespace_build_with_a_bare_destination_is_refused() {
        // `default` is a namespace like any other: its repositories are
        // `default/<app>`, and a bare name places nothing.
        assert!(validate_build_namespace("api", &spec("pickle://api", Some("default"))).is_err());
    }

    #[test]
    fn namespaced_build_into_its_own_namespace_is_accepted() {
        validate_build_namespace(
            "api",
            &spec("pickle://production/api:v1", Some("production")),
        )
        .unwrap();
        validate_build_namespace(
            "api",
            &spec("pickle://production/api/debug:v1", Some("production")),
        )
        .unwrap();
    }

    #[test]
    fn namespaced_build_into_another_namespace_is_refused() {
        let error =
            validate_build_namespace("api", &spec("pickle://staging/api:v1", Some("production")))
                .unwrap_err()
                .to_string();
        assert!(error.contains("staging"), "{error}");
    }

    #[test]
    fn namespaced_build_into_a_name_the_convention_cannot_place_is_refused() {
        for destination in ["pickle://cache/api:v1", "pickle://production//api:v1"] {
            assert!(
                validate_build_namespace("api", &spec(destination, Some("production"))).is_err(),
                "{destination}"
            );
        }
    }

    #[test]
    fn build_without_a_namespace_may_use_a_bare_destination() {
        validate_build_namespace("api", &spec("pickle://api:v1", None)).unwrap();
        validate_build_namespace("api", &spec("pickle://team-a/api:v1", None)).unwrap();
    }
}

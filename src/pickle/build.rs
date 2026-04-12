//! Build job execution — in-cluster image building.
//!
//! A build job runs a builder container with the build context mounted,
//! captures the resulting image layers, and pushes them to the Pickle
//! registry. For Phase 8 v1, builds use a simple builder (no layer
//! caching). Layer caching is a Phase 9 optimisation.

use crate::config::build::{BuildSpec, PickleDestination, parse_pickle_destination};
use crate::config::error::ConfigError;

/// Result of a build job.
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// The destination where the image was pushed.
    pub destination: PickleDestination,
    /// Number of layers produced.
    pub layers: usize,
    /// Total image size in bytes.
    pub size_bytes: u64,
}

/// Errors from build job execution.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("build context {path:?} does not exist")]
    ContextNotFound { path: std::path::PathBuf },

    #[error("dockerfile {path:?} not found in context")]
    DockerfileNotFound { path: String },

    #[error("destination validation failed: {0}")]
    InvalidDestination(#[from] ConfigError),

    #[error(
        "namespace mismatch: build is in namespace {build_ns:?} but destination would push to {dest_ns:?}"
    )]
    NamespaceMismatch { build_ns: String, dest_ns: String },

    #[error("builder failed: {reason}")]
    BuilderFailed { reason: String },

    #[error("push to pickle failed: {reason}")]
    PushFailed { reason: String },
}

/// Validate a build spec before execution.
///
/// Checks that the context directory and dockerfile exist, and that
/// the destination uses `pickle://` protocol.
pub fn validate_build(spec: &BuildSpec) -> Result<PickleDestination, BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;

    if spec.context.is_absolute() && !spec.context.exists() {
        return Err(BuildError::ContextNotFound {
            path: spec.context.clone(),
        });
    }

    // Check dockerfile exists if context is absolute and exists
    if spec.context.is_absolute() && spec.context.exists() {
        let dockerfile_path = spec.context.join(&spec.dockerfile);
        if !dockerfile_path.exists() {
            return Err(BuildError::DockerfileNotFound {
                path: spec.dockerfile.clone(),
            });
        }
    }

    Ok(dest)
}

/// Check that a build's namespace is allowed to push to the destination.
///
/// If the build has a namespace, the destination image name must not
/// conflict with another namespace's scope. For v1, we simply check
/// that the build declares a namespace (builds without a namespace
/// can push anywhere).
pub fn check_namespace_scope(
    spec: &BuildSpec,
    existing_namespaces: &[String],
) -> Result<(), BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;

    // If the image name contains a slash, treat the prefix as a namespace scope
    if let Some((ns_prefix, _)) = dest.name.split_once('/') {
        if let Some(build_ns) = &spec.namespace
            && ns_prefix != build_ns
        {
            return Err(BuildError::NamespaceMismatch {
                build_ns: build_ns.clone(),
                dest_ns: ns_prefix.to_string(),
            });
        }
        // Verify the namespace exists
        if !existing_namespaces.iter().any(|n| n == ns_prefix) {
            return Err(BuildError::NamespaceMismatch {
                build_ns: spec.namespace.clone().unwrap_or_default(),
                dest_ns: ns_prefix.to_string(),
            });
        }
    }

    Ok(())
}

/// Build arguments formatted for the builder container's environment.
pub fn build_args_to_env(spec: &BuildSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|(k, v)| format!("BUILD_ARG_{k}={v}"))
        .collect()
}

/// Resolve the full Dockerfile path from the build spec.
pub fn resolve_dockerfile(spec: &BuildSpec) -> std::path::PathBuf {
    spec.context.join(&spec.dockerfile)
}

/// Placeholder for the actual build execution.
///
/// In Phase 8 v1, this validates the spec and returns a placeholder
/// result. Full builder container integration (kaniko or custom)
/// is wired when the build CLI command triggers a deploy.
pub fn prepare_build(spec: &BuildSpec, _builder_image: &str) -> Result<BuildResult, BuildError> {
    let dest = validate_build(spec)?;

    Ok(BuildResult {
        destination: dest,
        layers: 0,
        size_bytes: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::*;

    fn spec_with_destination(dest: &str) -> BuildSpec {
        BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: dest.into(),
            args: BTreeMap::new(),
            namespace: None,
        }
    }

    #[test]
    fn validate_build_accepts_pickle_destination() {
        let spec = spec_with_destination("pickle://myapp:v1");
        let dest = validate_build(&spec).unwrap();
        assert_eq!(dest.name, "myapp");
        assert_eq!(dest.tag, "v1");
    }

    #[test]
    fn validate_build_rejects_non_pickle() {
        let spec = spec_with_destination("docker://myapp:v1");
        assert!(validate_build(&spec).is_err());
    }

    #[test]
    fn namespace_scope_allows_matching_prefix() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://production/myapp:v1".into(),
            args: BTreeMap::new(),
            namespace: Some("production".into()),
        };
        let namespaces = vec!["production".to_string(), "staging".to_string()];
        assert!(check_namespace_scope(&spec, &namespaces).is_ok());
    }

    #[test]
    fn namespace_scope_rejects_mismatched_prefix() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://staging/myapp:v1".into(),
            args: BTreeMap::new(),
            namespace: Some("production".into()),
        };
        let namespaces = vec!["production".to_string(), "staging".to_string()];
        let err = check_namespace_scope(&spec, &namespaces).unwrap_err();
        assert!(matches!(err, BuildError::NamespaceMismatch { .. }));
    }

    #[test]
    fn build_args_to_env_formats_correctly() {
        let mut args = BTreeMap::new();
        args.insert("VERSION".to_string(), "1.78".to_string());
        args.insert("FEATURES".to_string(), "ebpf".to_string());
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://app:v1".into(),
            args,
            namespace: None,
        };
        let env = build_args_to_env(&spec);
        assert_eq!(env.len(), 2);
        assert!(env.contains(&"BUILD_ARG_FEATURES=ebpf".to_string()));
        assert!(env.contains(&"BUILD_ARG_VERSION=1.78".to_string()));
    }

    #[test]
    fn resolve_dockerfile_joins_context_and_name() {
        let spec = BuildSpec {
            context: PathBuf::from("/src/myapp"),
            dockerfile: "Dockerfile.prod".into(),
            destination: "pickle://app:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
        };
        assert_eq!(
            resolve_dockerfile(&spec),
            Path::new("/src/myapp/Dockerfile.prod")
        );
    }

    #[test]
    fn prepare_build_returns_result_for_valid_spec() {
        let spec = spec_with_destination("pickle://myapp:v2");
        let result = prepare_build(&spec, "builder:latest").unwrap();
        assert_eq!(result.destination.name, "myapp");
        assert_eq!(result.destination.tag, "v2");
    }
}

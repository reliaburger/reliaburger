//! Build job execution — in-cluster image building via buildah.
//!
//! Builds OCI images from Dockerfiles using buildah, then pushes them
//! to the local Pickle registry. Buildah runs daemonless with
//! `--storage-driver vfs` for fully unprivileged, rootless builds.
//!
//! The build runs as a process job: two subprocess calls (`buildah bud`
//! to build, `buildah push` to push). No client libraries, no daemons.

use std::path::PathBuf;

use crate::config::build::{BuildSpec, PickleDestination, parse_pickle_destination};
use crate::config::error::ConfigError;

/// Default Pickle registry port (same as the Bun API port).
const DEFAULT_PICKLE_PORT: u16 = 9117;

/// A prepared buildah build, ready for execution as a process job.
///
/// Contains the CLI commands and arguments for both the build and
/// push steps. The caller runs these as subprocesses.
#[derive(Debug, Clone)]
pub struct BuildahJob {
    /// The buildah bud command and arguments.
    pub build_cmd: Vec<String>,
    /// The buildah push command and arguments.
    pub push_cmd: Vec<String>,
    /// Destination image reference.
    pub destination: PickleDestination,
    /// Local image tag (used between build and push).
    pub local_tag: String,
}

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
    ContextNotFound { path: PathBuf },

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
pub fn validate_build(spec: &BuildSpec) -> Result<PickleDestination, BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;

    if spec.context.is_absolute() && !spec.context.exists() {
        return Err(BuildError::ContextNotFound {
            path: spec.context.clone(),
        });
    }

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

/// Prepare a buildah build job from a BuildSpec.
///
/// Returns a `BuildahJob` containing the CLI commands for both the
/// build (`buildah bud`) and push (`buildah push`) steps. The caller
/// runs these as subprocesses via ProcessGrill or `tokio::process::Command`.
///
/// The `pickle_port` argument specifies the Pickle registry port
/// (default 9117). Buildah pushes to `localhost:{port}/{name}:{tag}`.
pub fn execute_build(spec: &BuildSpec, pickle_port: Option<u16>) -> Result<BuildahJob, BuildError> {
    let dest = validate_build(spec)?;
    let port = pickle_port.unwrap_or(DEFAULT_PICKLE_PORT);

    let local_tag = format!("localhost:{port}/{}:{}", dest.name, dest.tag);
    let build_cmd = buildah_build_args(spec, &local_tag);
    let push_cmd = buildah_push_args(&local_tag);

    Ok(BuildahJob {
        build_cmd,
        push_cmd,
        destination: dest,
        local_tag,
    })
}

/// Generate the `buildah bud` command arguments.
///
/// Produces a command like:
/// ```text
/// buildah bud --storage-driver vfs -f Dockerfile \
///   --platform linux/amd64,linux/arm64 --manifest localhost:9117/myapp:v1 .
/// ```
///
/// When multiple platforms are specified, buildah produces a manifest
/// list (OCI index) so the image works on mixed-architecture clusters.
fn buildah_build_args(spec: &BuildSpec, local_tag: &str) -> Vec<String> {
    let mut args = vec![
        "buildah".to_string(),
        "bud".to_string(),
        "--storage-driver".to_string(),
        "vfs".to_string(),
        "-f".to_string(),
        spec.dockerfile.clone(),
    ];

    // Platform targeting: single platform uses -t, multiple uses --manifest
    if spec.platform.len() > 1 {
        args.push("--platform".to_string());
        args.push(spec.platform.join(","));
        args.push("--manifest".to_string());
        args.push(local_tag.to_string());
    } else {
        if let Some(platform) = spec.platform.first() {
            args.push("--platform".to_string());
            args.push(platform.clone());
        }
        args.push("-t".to_string());
        args.push(local_tag.to_string());
    }

    for (key, value) in &spec.args {
        args.push("--build-arg".to_string());
        args.push(format!("{key}={value}"));
    }

    args.push(spec.context.to_string_lossy().to_string());

    args
}

/// Generate the `buildah push` command arguments.
///
/// Produces a command like:
/// ```text
/// buildah push --storage-driver vfs --tls-verify=false \
///   localhost:9117/myapp:v1 docker://localhost:9117/myapp:v1
/// ```
fn buildah_push_args(local_tag: &str) -> Vec<String> {
    vec![
        "buildah".to_string(),
        "push".to_string(),
        "--storage-driver".to_string(),
        "vfs".to_string(),
        "--tls-verify=false".to_string(),
        local_tag.to_string(),
        format!("docker://{local_tag}"),
    ]
}

/// Check that a build's namespace is allowed to push to the destination.
pub fn check_namespace_scope(
    spec: &BuildSpec,
    existing_namespaces: &[String],
) -> Result<(), BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;

    if let Some((ns_prefix, _)) = dest.name.split_once('/') {
        if let Some(build_ns) = &spec.namespace
            && ns_prefix != build_ns
        {
            return Err(BuildError::NamespaceMismatch {
                build_ns: build_ns.clone(),
                dest_ns: ns_prefix.to_string(),
            });
        }
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
pub fn resolve_dockerfile(spec: &BuildSpec) -> PathBuf {
    spec.context.join(&spec.dockerfile)
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
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
        }
    }

    fn spec_with_args(args: BTreeMap<String, String>) -> BuildSpec {
        BuildSpec {
            context: PathBuf::from("./src"),
            dockerfile: "Dockerfile.prod".into(),
            destination: "pickle://myapp:v1".into(),
            args,
            namespace: None,
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
        }
    }

    // --- validate_build ---

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

    // --- execute_build ---

    #[test]
    fn execute_build_produces_valid_job() {
        let spec = spec_with_destination("pickle://myapp:v2");
        let job = execute_build(&spec, Some(9117)).unwrap();
        assert_eq!(job.destination.name, "myapp");
        assert_eq!(job.destination.tag, "v2");
        assert_eq!(job.local_tag, "localhost:9117/myapp:v2");
    }

    #[test]
    fn execute_build_uses_default_port() {
        let spec = spec_with_destination("pickle://app:latest");
        let job = execute_build(&spec, None).unwrap();
        assert!(job.local_tag.contains("9117"));
    }

    #[test]
    fn execute_build_rejects_missing_context() {
        let spec = BuildSpec {
            context: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://app:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
            platform: vec!["linux/amd64".into()],
        };
        let err = execute_build(&spec, None).unwrap_err();
        assert!(matches!(err, BuildError::ContextNotFound { .. }));
    }

    // --- buildah_build_args ---

    #[test]
    fn buildah_build_cmd_uses_vfs_storage() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, None).unwrap();
        assert!(job.build_cmd.contains(&"--storage-driver".to_string()));
        assert!(job.build_cmd.contains(&"vfs".to_string()));
    }

    #[test]
    fn buildah_build_cmd_includes_dockerfile() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile.prod".into(),
            destination: "pickle://app:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
            platform: vec!["linux/amd64".into()],
        };
        let job = execute_build(&spec, None).unwrap();
        let f_idx = job.build_cmd.iter().position(|a| a == "-f").unwrap();
        assert_eq!(job.build_cmd[f_idx + 1], "Dockerfile.prod");
    }

    #[test]
    fn buildah_build_cmd_includes_build_args() {
        let mut args = BTreeMap::new();
        args.insert("VERSION".to_string(), "1.78".to_string());
        args.insert("FEATURES".to_string(), "ebpf".to_string());
        let spec = spec_with_args(args);
        let job = execute_build(&spec, None).unwrap();

        let build_arg_count = job
            .build_cmd
            .iter()
            .filter(|a| a.as_str() == "--build-arg")
            .count();
        assert_eq!(build_arg_count, 2);
        assert!(job.build_cmd.iter().any(|a| a == "VERSION=1.78"));
        assert!(job.build_cmd.iter().any(|a| a == "FEATURES=ebpf"));
    }

    #[test]
    fn buildah_build_cmd_ends_with_context() {
        let spec = BuildSpec {
            context: PathBuf::from("./myproject"),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://app:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
            platform: vec!["linux/amd64".into()],
        };
        let job = execute_build(&spec, None).unwrap();
        assert_eq!(job.build_cmd.last().unwrap(), "./myproject");
    }

    // --- buildah_push_args ---

    #[test]
    fn buildah_push_cmd_targets_pickle() {
        let spec = spec_with_destination("pickle://myapp:v3");
        let job = execute_build(&spec, Some(5000)).unwrap();
        assert!(
            job.push_cmd
                .contains(&"localhost:5000/myapp:v3".to_string())
        );
        assert!(
            job.push_cmd
                .contains(&"docker://localhost:5000/myapp:v3".to_string())
        );
    }

    #[test]
    fn buildah_push_cmd_uses_vfs_storage() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, None).unwrap();
        assert!(job.push_cmd.contains(&"--storage-driver".to_string()));
        assert!(job.push_cmd.contains(&"vfs".to_string()));
    }

    #[test]
    fn buildah_push_cmd_disables_tls() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, None).unwrap();
        assert!(job.push_cmd.contains(&"--tls-verify=false".to_string()));
    }

    // --- namespace scoping ---

    #[test]
    fn namespace_scope_allows_matching_prefix() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://production/myapp:v1".into(),
            args: BTreeMap::new(),
            namespace: Some("production".into()),
            platform: vec![],
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
            platform: vec![],
        };
        let namespaces = vec!["production".to_string(), "staging".to_string()];
        assert!(matches!(
            check_namespace_scope(&spec, &namespaces),
            Err(BuildError::NamespaceMismatch { .. })
        ));
    }

    // --- helpers ---

    #[test]
    fn build_args_to_env_formats_correctly() {
        let mut args = BTreeMap::new();
        args.insert("VERSION".to_string(), "1.78".to_string());
        args.insert("FEATURES".to_string(), "ebpf".to_string());
        let spec = spec_with_args(args);
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
            platform: vec![],
        };
        assert_eq!(
            resolve_dockerfile(&spec),
            Path::new("/src/myapp/Dockerfile.prod")
        );
    }

    // --- multi-platform ---

    #[test]
    fn buildah_multi_platform_uses_manifest_flag() {
        let spec = spec_with_destination("pickle://app:v1");
        // Default spec_with_destination uses both amd64 + arm64
        let job = execute_build(&spec, None).unwrap();
        assert!(job.build_cmd.contains(&"--manifest".to_string()));
        assert!(job.build_cmd.contains(&"--platform".to_string()));
        assert!(job.build_cmd.iter().any(|a| a == "linux/amd64,linux/arm64"));
        // Multi-platform should NOT use -t
        assert!(!job.build_cmd.contains(&"-t".to_string()));
    }

    #[test]
    fn buildah_single_platform_uses_tag_flag() {
        let spec = BuildSpec {
            context: PathBuf::from("."),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://app:v1".into(),
            args: BTreeMap::new(),
            namespace: None,
            platform: vec!["linux/amd64".into()],
        };
        let job = execute_build(&spec, None).unwrap();
        assert!(job.build_cmd.contains(&"-t".to_string()));
        assert!(job.build_cmd.contains(&"--platform".to_string()));
        assert!(job.build_cmd.iter().any(|a| a == "linux/amd64"));
        // Single platform should NOT use --manifest
        assert!(!job.build_cmd.contains(&"--manifest".to_string()));
    }
}

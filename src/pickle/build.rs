//! Build job execution — in-cluster image building via buildah.
//!
//! The build flow:
//! 1. CLI tars the build context and uploads it to Pickle as a blob
//! 2. Build job is scheduled to a node (like any other job)
//! 3. Target node downloads the context blob from Pickle, extracts it
//! 4. Buildah builds the image and pushes it back to Pickle
//!
//! This design means the build context doesn't need to be on a shared
//! filesystem — Pickle (which every node already talks to) handles
//! the transfer.

use std::path::{Component, Path, PathBuf};

use crate::config::build::{BuildSpec, PickleDestination, parse_pickle_destination};
use crate::config::error::ConfigError;

/// Ceiling on the number of entries a context archive may contain.
/// Generous for any real build context; small enough that an
/// entry-bomb archive cannot exhaust inodes or directory entries.
pub const MAX_CONTEXT_ENTRIES: usize = 65_536;

/// Default Pickle registry port.
///
/// X1 regression note: this used to be 9117 — the *Bun API* port,
/// which has no `/v2` routes — so context uploads always 404ed.
/// It must match `ImagesSection::default().registry_port`.
const DEFAULT_PICKLE_PORT: u16 = 5050;

/// A prepared buildah build, ready for execution as a process job.
///
/// Contains the CLI commands and arguments for both the build and
/// push steps. The caller runs these as subprocesses.
#[derive(Debug, Clone)]
pub struct BuildahJob {
    /// The buildah bud command and arguments.
    pub build_cmd: Vec<String>,
    /// The buildah push command and arguments.
    ///
    /// Kept for display and for the delegated single-node path. The clustered
    /// runner does NOT run this: a `docker://` push cannot authenticate against
    /// Pickle (see [`buildah_push_to_oci_args`] and B1). It exports an OCI
    /// layout and uploads it through the bearer-carrying registry client
    /// instead.
    pub push_cmd: Vec<String>,
    /// Destination image reference.
    pub destination: PickleDestination,
    /// Local image tag (used between build and push).
    pub local_tag: String,
    /// Digest of the context blob in Pickle (for the build node to download).
    pub context_blob_digest: String,
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

    #[error("failed to tar build context: {reason}")]
    TarFailed { reason: String },

    #[error("builder failed: {reason}")]
    BuilderFailed { reason: String },

    #[error("push to pickle failed: {reason}")]
    PushFailed { reason: String },

    #[error("dockerfile path {path:?} escapes the build context")]
    DockerfileOutsideContext { path: String },

    #[error("build context exceeds the {limit_bytes} byte limit")]
    ContextTooLarge { limit_bytes: u64 },

    #[error("build context entry {path:?} rejected: {reason}")]
    UnsafeContextEntry { path: String, reason: String },

    #[error("build context has more than {limit} entries")]
    ContextTooManyEntries { limit: usize },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate a build spec before execution.
pub fn validate_build(spec: &BuildSpec) -> Result<PickleDestination, BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;
    validate_dockerfile_path(&spec.dockerfile)?;

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

/// Tar a build context directory into bytes.
///
/// Creates a tar archive of the context directory. The Dockerfile and
/// all source files are included. The archive is what gets uploaded
/// to Pickle as a blob and downloaded by the build node.
pub fn tar_context(context_dir: &Path) -> Result<Vec<u8>, BuildError> {
    if !context_dir.exists() {
        return Err(BuildError::ContextNotFound {
            path: context_dir.to_path_buf(),
        });
    }

    let mut archive = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut archive);
        tar.append_dir_all(".", context_dir)
            .map_err(|e| BuildError::TarFailed {
                reason: format!("failed to add {}: {e}", context_dir.display()),
            })?;
        tar.finish().map_err(|e| BuildError::TarFailed {
            reason: format!("failed to finalise tar: {e}"),
        })?;
    }
    Ok(archive)
}

/// Compute the SHA-256 digest of data, in the format Pickle expects.
pub fn digest_of(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    format!("sha256:{hash:x}")
}

/// Prepare a buildah build job from a BuildSpec.
///
/// The `context_digest` is the Pickle blob digest of the tarred build
/// context (uploaded by the CLI before scheduling the job). The build
/// node downloads this blob, extracts it, and runs buildah.
/// `registry_over_tls` decides whether the push verifies the registry's
/// certificate (O2).
///
/// The `push_cmd` here targets `docker://`, which the clustered runner no
/// longer uses (it cannot authenticate — B1); the runner exports an OCI layout
/// via [`buildah_push_to_oci_args`] and uploads it with the service-token
/// bearer instead. This command is retained for `relish`'s display and the
/// standalone/loopback case.
pub fn execute_build(
    spec: &BuildSpec,
    context_digest: &str,
    pickle_port: Option<u16>,
    registry_over_tls: bool,
) -> Result<BuildahJob, BuildError> {
    let dest = validate_build(spec)?;
    let port = pickle_port.unwrap_or(DEFAULT_PICKLE_PORT);

    let local_tag = format!("localhost:{port}/{}:{}", dest.name, dest.tag);
    let build_cmd = buildah_build_args(spec, &local_tag);
    let push_cmd = buildah_push_args(&local_tag, registry_over_tls);

    Ok(BuildahJob {
        build_cmd,
        push_cmd,
        destination: dest,
        local_tag,
        context_blob_digest: context_digest.to_string(),
    })
}

/// Generate the `buildah bud` command arguments.
///
/// The context path is `/tmp/reliaburger-build/{digest}/` — the build
/// node extracts the context blob there before running buildah.
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

    // Context directory — the extracted blob on the build node
    args.push(".".to_string());

    args
}

/// Generate the `buildah push` command arguments.
///
/// `tls_verify` mirrors how the destination registry actually serves
/// (O2). It was hardcoded off, which is right for the plaintext loopback
/// default and wrong the moment the registry runs over TLS: a build would
/// push its image without checking the certificate it was pushing to.
fn buildah_push_args(local_tag: &str, tls_verify: bool) -> Vec<String> {
    vec![
        "buildah".to_string(),
        "push".to_string(),
        "--storage-driver".to_string(),
        "vfs".to_string(),
        format!("--tls-verify={tls_verify}"),
        local_tag.to_string(),
        format!("docker://{local_tag}"),
    ]
}

/// Generate a `buildah push` that exports the built image to a *local* OCI
/// image layout directory (`oci:{dir}:{tag}`) instead of pushing to a
/// `docker://` registry (B1).
///
/// Pickle authorises registry writes with the internal service token presented
/// as a *bearer*; `buildah push` can only offer `--creds` (HTTP Basic), which
/// Pickle does not accept and never challenges for — so a clustered `docker://`
/// push simply 401s. The clustered runner uses this command to write a local
/// layout, then uploads it to the registry through the bearer-carrying client.
/// A local layout export needs no TLS and no credentials.
pub fn buildah_push_to_oci_args(local_tag: &str, oci_layout_dir: &str, tag: &str) -> Vec<String> {
    vec![
        "buildah".to_string(),
        "push".to_string(),
        "--storage-driver".to_string(),
        "vfs".to_string(),
        local_tag.to_string(),
        format!("oci:{oci_layout_dir}:{tag}"),
    ]
}

/// Build the URL to download a context blob from Pickle.
///
/// The build node fetches this before running buildah. `scheme` is the
/// registry's own scheme (`"http"` or `"https"`), threaded from config
/// rather than hardcoded: against a TLS registry the old `http://` URLs
/// simply failed, and where they worked they moved the build context —
/// which is the caller's source tree — in the clear (O2).
pub fn context_download_url(scheme: &str, pickle_port: u16, digest: &str) -> String {
    // Uses the OCI blob GET endpoint. The "name" is _buildcontext
    // (a reserved namespace that doesn't clash with real images).
    format!("{scheme}://localhost:{pickle_port}/v2/_buildcontext/blobs/{digest}")
}

/// Build the URL to download a context blob from a specific node's
/// Pickle registry (`address` is `host:port`). Used by a delegated
/// builder to fetch the context from the entry node — the address is
/// derived from cluster membership, never from the request body (JOB2).
pub fn context_download_url_at(scheme: &str, address: &str, digest: &str) -> String {
    format!("{scheme}://{address}/v2/_buildcontext/blobs/{digest}")
}

/// Build the URL to upload a context blob to Pickle.
///
/// The CLI uploads the tarred context here before scheduling the build.
pub fn context_upload_url(scheme: &str, pickle_port: u16, digest: &str) -> String {
    format!("{scheme}://localhost:{pickle_port}/v2/_buildcontext/blobs/uploads/?digest={digest}")
}

/// Build the URL to upload a context blob to a specific node's Pickle
/// registry (`address` is `host:port`). Used by a delegating node to
/// transfer the context to the chosen builder (12b.2, JOB5): bare
/// `_buildcontext` blobs have no manifest, so nothing replicates them
/// — the delegator copies the blob across before the run request. The
/// address derives from cluster membership, never from request data.
pub fn context_upload_url_at(scheme: &str, address: &str, digest: &str) -> String {
    format!("{scheme}://{address}/v2/_buildcontext/blobs/uploads/?digest={digest}")
}

// ---------------------------------------------------------------------------
// OCI image-layout upload (B1)
//
// A clustered `buildah push` to `docker://` cannot authenticate — Pickle
// authorises writes by the internal service token as a *bearer*, and buildah
// only offers `--creds` (HTTP Basic), which Pickle never accepts and never
// challenges for. So the runner exports the image to a local OCI layout and
// uploads it to the local registry through the bearer-carrying registry
// client. These helpers build the registry URLs and read the layout's index.
// ---------------------------------------------------------------------------

/// The monolithic-blob upload URL for the local registry: a single `POST`
/// with the blob body and its `digest` query stores a content-addressed blob.
pub fn oci_blob_upload_url(
    scheme: &str,
    pickle_port: u16,
    repository: &str,
    digest: &str,
) -> String {
    format!("{scheme}://localhost:{pickle_port}/v2/{repository}/blobs/uploads/?digest={digest}")
}

/// The manifest `PUT` URL for the local registry. `reference` is a tag or a
/// `sha256:` digest.
pub fn oci_manifest_put_url(
    scheme: &str,
    pickle_port: u16,
    repository: &str,
    reference: &str,
) -> String {
    format!("{scheme}://localhost:{pickle_port}/v2/{repository}/manifests/{reference}")
}

/// The top image (or image index) an OCI layout's `index.json` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciLayoutTop {
    /// Digest of the top manifest, e.g. `sha256:…`.
    pub digest: String,
    /// Its media type, used as the `Content-Type` on the manifest `PUT`.
    pub media_type: Option<String>,
}

/// Parse an OCI layout `index.json`, returning the single top-level manifest
/// descriptor buildah wrote for the pushed tag.
///
/// A layout exported for one tag carries exactly one top manifest — either an
/// image manifest (single platform) or an image index (multi-platform). All
/// blobs it transitively references live under `blobs/sha256/`, so the caller
/// uploads every blob first and then `PUT`s this manifest by tag; Pickle's
/// manifest validation then finds each referenced blob (config, layers, or
/// sub-manifests) already present.
pub fn parse_oci_index(index_json: &[u8]) -> Result<OciLayoutTop, BuildError> {
    #[derive(serde::Deserialize)]
    struct Descriptor {
        digest: String,
        #[serde(rename = "mediaType")]
        media_type: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Index {
        manifests: Vec<Descriptor>,
    }
    let index: Index = serde_json::from_slice(index_json).map_err(|e| BuildError::PushFailed {
        reason: format!("oci layout index.json is not valid json: {e}"),
    })?;
    let top = index
        .manifests
        .into_iter()
        .next()
        .ok_or_else(|| BuildError::PushFailed {
            reason: "oci layout index.json lists no manifests".to_string(),
        })?;
    Ok(OciLayoutTop {
        digest: top.digest,
        media_type: top.media_type,
    })
}

/// The namespace and image name a `pickle://` destination targets.
///
/// A bare `pickle://name:tag` targets the `default` namespace; a
/// `pickle://ns/name:tag` targets namespace `ns`. The namespace is taken from
/// the destination itself — never from a self-declared build field — so it can
/// be checked against the authenticated caller's token scope before the build
/// runs. Used by the build submit handler to bind an image push to its owner.
pub fn destination_scope(spec: &BuildSpec) -> Result<(String, String), BuildError> {
    let dest = parse_pickle_destination(&spec.destination)?;
    let (namespace, image) = match dest.name.split_once('/') {
        Some((ns, image)) => (ns.to_string(), image.to_string()),
        None => ("default".to_string(), dest.name.clone()),
    };
    Ok((namespace, image))
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

/// Check that a Dockerfile path is a plain relative path — no absolute
/// prefix, no `..` — so joining it onto a context directory can never
/// resolve outside that directory (JOB6).
pub fn validate_dockerfile_path(dockerfile: &str) -> Result<(), BuildError> {
    let path = Path::new(dockerfile);
    let confined = !dockerfile.is_empty()
        && path.is_relative()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if confined {
        Ok(())
    } else {
        Err(BuildError::DockerfileOutsideContext {
            path: dockerfile.to_string(),
        })
    }
}

/// Resolve a Dockerfile inside an extracted context directory and prove
/// it stays inside it. Canonicalises both sides, so even a path that
/// sneaks past the component check (or a directory swapped for a
/// symlink) cannot point Buildah at a file outside the context.
pub fn confine_dockerfile(context_dir: &Path, dockerfile: &str) -> Result<PathBuf, BuildError> {
    validate_dockerfile_path(dockerfile)?;
    let context = context_dir.canonicalize()?;
    let resolved =
        context
            .join(dockerfile)
            .canonicalize()
            .map_err(|_| BuildError::DockerfileNotFound {
                path: dockerfile.to_string(),
            })?;
    if resolved.starts_with(&context) {
        Ok(resolved)
    } else {
        Err(BuildError::DockerfileOutsideContext {
            path: dockerfile.to_string(),
        })
    }
}

/// Map a tar entry path to a safe relative path under the extraction
/// root: only normal components (and `.`) survive; absolute paths and
/// `..` are rejected outright.
fn sanitise_entry_path(path: &Path) -> Result<PathBuf, BuildError> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            _ => {
                return Err(BuildError::UnsafeContextEntry {
                    path: path.display().to_string(),
                    reason: "path escapes the extraction directory".to_string(),
                });
            }
        }
    }
    Ok(safe)
}

/// Copy at most `budget` bytes from `reader` to `writer`, returning the
/// number written. Counting *written* bytes (not header sizes) is what
/// stops a sparse-file bomb: the archive can claim any size it likes,
/// but the bytes that actually land on disk are bounded.
fn copy_capped<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
    budget: u64,
    limit_bytes: u64,
) -> Result<u64, BuildError> {
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            return Ok(written);
        }
        written += n as u64;
        if written > budget {
            return Err(BuildError::ContextTooLarge { limit_bytes });
        }
        writer.write_all(&buffer[..n])?;
    }
}

/// Unpack a build-context tar into `dest` with hard bounds (JOB6).
///
/// Only regular files and directories are extracted — symlinks, hard
/// links, devices and FIFOs are rejected, so nothing inside the context
/// can point outside it. Entry paths must stay under `dest`, cumulative
/// *written* bytes are capped at `max_bytes`, the entry count is capped
/// at [`MAX_CONTEXT_ENTRIES`], and setuid/setgid/sticky bits are
/// stripped from file modes.
pub fn unpack_context<R: std::io::Read>(
    reader: R,
    dest: &Path,
    max_bytes: u64,
) -> Result<(), BuildError> {
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(reader);
    let mut budget = max_bytes;
    let mut entries_seen = 0usize;

    for entry in archive.entries()? {
        let mut entry = entry?;
        entries_seen += 1;
        if entries_seen > MAX_CONTEXT_ENTRIES {
            return Err(BuildError::ContextTooManyEntries {
                limit: MAX_CONTEXT_ENTRIES,
            });
        }

        let raw_path = entry.path()?.into_owned();
        let safe_path = sanitise_entry_path(&raw_path)?;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(dest.join(&safe_path))?;
            }
            tar::EntryType::Regular => {
                let out_path = dest.join(&safe_path);
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::File::create(&out_path)?;
                let written = copy_capped(&mut entry, &mut out, budget, max_bytes)?;
                budget -= written;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Keep rwx bits only: a context must never install a
                    // setuid binary onto the build node.
                    let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
                    std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
                }
            }
            other => {
                return Err(BuildError::UnsafeContextEntry {
                    path: raw_path.display().to_string(),
                    reason: format!("entry type {other:?} is not allowed in a build context"),
                });
            }
        }
    }
    Ok(())
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

    // --- tar_context ---

    #[test]
    fn tar_context_creates_archive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine\n").unwrap();
        std::fs::write(dir.path().join("app.py"), "print('hello')\n").unwrap();

        let archive = tar_context(dir.path()).unwrap();
        assert!(!archive.is_empty());

        // Verify we can list entries
        let mut tar = tar::Archive::new(archive.as_slice());
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.path().ok().map(|p| p.to_string_lossy().to_string()))
            .collect();
        assert!(names.iter().any(|n| n.contains("Dockerfile")));
        assert!(names.iter().any(|n| n.contains("app.py")));
    }

    #[test]
    fn tar_context_rejects_missing_dir() {
        let result = tar_context(Path::new("/nonexistent/build/context"));
        assert!(matches!(result, Err(BuildError::ContextNotFound { .. })));
    }

    // --- digest_of ---

    #[test]
    fn digest_of_is_deterministic() {
        let d1 = digest_of(b"hello world");
        let d2 = digest_of(b"hello world");
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
    }

    #[test]
    fn digest_of_different_data_differs() {
        assert_ne!(digest_of(b"hello"), digest_of(b"world"));
    }

    // --- execute_build ---

    #[test]
    fn execute_build_produces_valid_job() {
        let spec = spec_with_destination("pickle://myapp:v2");
        let job = execute_build(&spec, "sha256:abc123", Some(9117), false).unwrap();
        assert_eq!(job.destination.name, "myapp");
        assert_eq!(job.destination.tag, "v2");
        assert_eq!(job.local_tag, "localhost:9117/myapp:v2");
        assert_eq!(job.context_blob_digest, "sha256:abc123");
    }

    #[test]
    fn execute_build_uses_default_port() {
        let spec = spec_with_destination("pickle://app:latest");
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        // Defaults to the Pickle registry port (5050), not the Bun API
        // port — see the X1 regression note on DEFAULT_PICKLE_PORT.
        assert!(job.local_tag.contains("5050"));
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
        let err = execute_build(&spec, "sha256:abc", None, false).unwrap_err();
        assert!(matches!(err, BuildError::ContextNotFound { .. }));
    }

    // --- buildah_build_args ---

    #[test]
    fn buildah_build_cmd_uses_vfs_storage() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
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
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        let f_idx = job.build_cmd.iter().position(|a| a == "-f").unwrap();
        assert_eq!(job.build_cmd[f_idx + 1], "Dockerfile.prod");
    }

    #[test]
    fn buildah_build_cmd_includes_build_args() {
        let mut args = BTreeMap::new();
        args.insert("VERSION".to_string(), "1.78".to_string());
        args.insert("FEATURES".to_string(), "ebpf".to_string());
        let spec = spec_with_args(args);
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();

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
    fn buildah_build_cmd_ends_with_context_dot() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        assert_eq!(job.build_cmd.last().unwrap(), ".");
    }

    // --- buildah_push_args ---

    #[test]
    fn buildah_push_cmd_targets_pickle() {
        let spec = spec_with_destination("pickle://myapp:v3");
        let job = execute_build(&spec, "sha256:abc", Some(5000), false).unwrap();
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
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        assert!(job.push_cmd.contains(&"--storage-driver".to_string()));
        assert!(job.push_cmd.contains(&"vfs".to_string()));
    }

    #[test]
    fn buildah_push_skips_tls_verification_against_a_plaintext_registry() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        assert!(job.push_cmd.contains(&"--tls-verify=false".to_string()));
    }

    /// O2: `--tls-verify=false` was hardcoded, so a build pushed to a TLS
    /// registry without ever checking the certificate it was pushing to.
    #[test]
    fn buildah_push_verifies_tls_against_a_secured_registry() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, "sha256:abc", None, true).unwrap();
        assert!(job.push_cmd.contains(&"--tls-verify=true".to_string()));
        assert!(!job.push_cmd.contains(&"--tls-verify=false".to_string()));
    }

    /// B1: the clustered runner exports the image to a local OCI layout dir
    /// (`oci:{dir}:{tag}`) instead of a `docker://` push, because buildah
    /// cannot present the service-token bearer that Pickle requires for writes.
    #[test]
    fn buildah_push_to_oci_exports_a_local_layout() {
        let cmd = buildah_push_to_oci_args("localhost:5000/myapp:v3", "/build/oci", "v3");
        assert!(cmd.contains(&"localhost:5000/myapp:v3".to_string()));
        assert!(cmd.contains(&"oci:/build/oci:v3".to_string()));
        assert!(!cmd.iter().any(|a| a.starts_with("docker://")));
        assert!(!cmd.iter().any(|a| a.starts_with("--tls-verify")));
    }

    // --- context URLs ---

    #[test]
    fn context_download_url_format() {
        let url = context_download_url("http", 9117, "sha256:abc123");
        assert_eq!(
            url,
            "http://localhost:9117/v2/_buildcontext/blobs/sha256:abc123"
        );
    }

    #[test]
    fn context_upload_url_targets_the_registry_port() {
        let url = context_upload_url("http", 5050, "sha256:abc123");
        assert_eq!(
            url,
            "http://localhost:5050/v2/_buildcontext/blobs/uploads/?digest=sha256:abc123"
        );
    }

    /// O2: every context URL was hardcoded `http://`, so against a TLS
    /// registry a delegated build simply failed — and where plaintext did
    /// reach a listener, the context (the caller's source tree) crossed the
    /// network in clear.
    #[test]
    fn context_urls_follow_the_registrys_scheme() {
        assert!(
            context_download_url("https", 5050, "sha256:abc")
                .starts_with("https://localhost:5050/")
        );
        assert!(
            context_upload_url("https", 5050, "sha256:abc").starts_with("https://localhost:5050/")
        );
        assert!(
            context_download_url_at("https", "10.0.1.5:5050", "sha256:abc")
                .starts_with("https://10.0.1.5:5050/")
        );
        assert!(
            context_upload_url_at("https", "10.0.1.5:5050", "sha256:abc")
                .starts_with("https://10.0.1.5:5050/")
        );
    }

    // --- OCI layout upload (B1) ---

    #[test]
    fn oci_upload_urls_target_the_local_registry() {
        assert_eq!(
            oci_blob_upload_url("http", 5050, "team-a/api", "sha256:abc"),
            "http://localhost:5050/v2/team-a/api/blobs/uploads/?digest=sha256:abc"
        );
        assert_eq!(
            oci_manifest_put_url("https", 5050, "myapp", "v1"),
            "https://localhost:5050/v2/myapp/manifests/v1"
        );
    }

    #[test]
    fn parse_oci_index_reads_the_top_manifest() {
        let index = br#"{
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                    "size": 42
                }
            ]
        }"#;
        let top = parse_oci_index(index).unwrap();
        assert_eq!(
            top.digest,
            "sha256:1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            top.media_type.as_deref(),
            Some("application/vnd.oci.image.index.v1+json")
        );
    }

    #[test]
    fn parse_oci_index_rejects_an_empty_layout() {
        let index = br#"{ "schemaVersion": 2, "manifests": [] }"#;
        assert!(matches!(
            parse_oci_index(index),
            Err(BuildError::PushFailed { .. })
        ));
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

    // --- destination scope / push authorisation (Phase 16 B) ---

    #[test]
    fn destination_scope_uses_default_for_bare_names() {
        let spec = spec_with_destination("pickle://victim-app:v1");
        assert_eq!(
            destination_scope(&spec).unwrap(),
            ("default".to_string(), "victim-app".to_string())
        );
    }

    #[test]
    fn destination_scope_reads_namespace_prefix() {
        let spec = spec_with_destination("pickle://team-b/api:v1");
        assert_eq!(
            destination_scope(&spec).unwrap(),
            ("team-b".to_string(), "api".to_string())
        );
    }

    #[test]
    fn scoped_token_cannot_push_across_namespaces_or_to_bare_names() {
        use crate::sesame::auth::{AuthContext, authorize_scoped};
        // A Deployer scoped to `team-a` only. `spec_with_destination` leaves the
        // build's own `namespace` field None, so this proves the gate reads the
        // destination, not a self-declared field.
        let ctx = AuthContext {
            token_name: "ci".into(),
            principal_id: "ci-1".into(),
            role: crate::sesame::types::ApiRole::Deployer,
            scoped_apps: None,
            scoped_namespaces: Some(vec!["team-a".into()]),
        };

        // Pushing into another namespace's repository is refused.
        let (ns, image) =
            destination_scope(&spec_with_destination("pickle://team-b/api:v1")).unwrap();
        assert!(authorize_scoped(Some(&ctx), &image, &ns).is_err());

        // A bare name resolves to `default` — the old bypass — and is refused too.
        let (ns, image) = destination_scope(&spec_with_destination("pickle://victim:v1")).unwrap();
        assert!(authorize_scoped(Some(&ctx), &image, &ns).is_err());

        // The caller's own namespace is allowed.
        let (ns, image) =
            destination_scope(&spec_with_destination("pickle://team-a/api:v1")).unwrap();
        assert!(authorize_scoped(Some(&ctx), &image, &ns).is_ok());
    }

    #[test]
    fn unscoped_token_pushes_anywhere() {
        use crate::sesame::auth::{AuthContext, authorize_scoped};
        let ctx = AuthContext {
            token_name: "admin".into(),
            principal_id: "admin-1".into(),
            role: crate::sesame::types::ApiRole::Deployer,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        let (ns, image) =
            destination_scope(&spec_with_destination("pickle://anything/api:v1")).unwrap();
        assert!(authorize_scoped(Some(&ctx), &image, &ns).is_ok());
    }

    // --- multi-platform ---

    #[test]
    fn buildah_multi_platform_uses_manifest_flag() {
        let spec = spec_with_destination("pickle://app:v1");
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        assert!(job.build_cmd.contains(&"--manifest".to_string()));
        assert!(job.build_cmd.contains(&"--platform".to_string()));
        assert!(job.build_cmd.iter().any(|a| a == "linux/amd64,linux/arm64"));
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
        let job = execute_build(&spec, "sha256:abc", None, false).unwrap();
        assert!(job.build_cmd.contains(&"-t".to_string()));
        assert!(job.build_cmd.contains(&"--platform".to_string()));
        assert!(job.build_cmd.iter().any(|a| a == "linux/amd64"));
        assert!(!job.build_cmd.contains(&"--manifest".to_string()));
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

    // --- dockerfile confinement (JOB6) ---

    #[test]
    fn dockerfile_path_accepts_plain_relative_paths() {
        assert!(validate_dockerfile_path("Dockerfile").is_ok());
        assert!(validate_dockerfile_path("docker/Dockerfile.prod").is_ok());
        assert!(validate_dockerfile_path("./Dockerfile").is_ok());
    }

    #[test]
    fn dockerfile_path_rejects_traversal_and_absolute() {
        for bad in ["../../outside", "a/../../b", "/etc/passwd", ""] {
            assert!(
                matches!(
                    validate_dockerfile_path(bad),
                    Err(BuildError::DockerfileOutsideContext { .. })
                ),
                "dockerfile {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_build_rejects_a_traversal_dockerfile() {
        let mut spec = spec_with_destination("pickle://app:v1");
        spec.dockerfile = "../../outside".into();
        assert!(matches!(
            validate_build(&spec),
            Err(BuildError::DockerfileOutsideContext { .. })
        ));
    }

    #[test]
    fn confine_dockerfile_accepts_a_file_inside_the_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let resolved = confine_dockerfile(dir.path(), "Dockerfile").unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn confine_dockerfile_rejects_a_symlink_escape() {
        // A symlinked directory inside the context pointing outside it:
        // the component check passes, canonicalisation catches it.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();
        assert!(matches!(
            confine_dockerfile(dir.path(), "sub/Dockerfile"),
            Err(BuildError::DockerfileOutsideContext { .. })
        ));
    }

    // --- bounded, sandboxed extraction (JOB6) ---

    /// Build an in-memory tar of (path, mode, content) regular files.
    fn tar_of(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            for (path, mode, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append_data(&mut header, path, *content).unwrap();
            }
            builder.finish().unwrap();
        }
        archive
    }

    #[test]
    fn unpack_context_extracts_files_and_dirs() {
        let tar = tar_of(&[
            ("Dockerfile", 0o644, b"FROM scratch\n"),
            ("src/main.py", 0o644, b"print('hi')\n"),
        ]);
        let dest = tempfile::tempdir().unwrap();
        unpack_context(tar.as_slice(), dest.path(), 1024 * 1024).unwrap();
        assert!(dest.path().join("Dockerfile").is_file());
        assert!(dest.path().join("src/main.py").is_file());
    }

    #[test]
    fn unpack_context_rejects_parent_traversal_entries() {
        // tar::Builder refuses `..` in set_path, so write the raw GNU
        // header name bytes the way an attacker's tar writer would.
        let mut tar = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar);
            let content = b"pwned";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            let name = b"../evil";
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            builder.append(&header, &content[..]).unwrap();
            builder.finish().unwrap();
        }
        let dest = tempfile::tempdir().unwrap();
        assert!(matches!(
            unpack_context(tar.as_slice(), dest.path(), 1024 * 1024),
            Err(BuildError::UnsafeContextEntry { .. })
        ));
        assert!(!dest.path().parent().unwrap().join("evil").exists());
    }

    #[test]
    fn unpack_context_rejects_symlink_entries() {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_link(&mut header, "link", "/etc/passwd")
                .unwrap();
            builder.finish().unwrap();
        }
        let dest = tempfile::tempdir().unwrap();
        assert!(matches!(
            unpack_context(archive.as_slice(), dest.path(), 1024 * 1024),
            Err(BuildError::UnsafeContextEntry { .. })
        ));
    }

    #[test]
    fn unpack_context_stops_at_the_byte_cap() {
        let big = vec![0u8; 8 * 1024];
        let tar = tar_of(&[("a", 0o644, &big), ("b", 0o644, &big)]);
        let dest = tempfile::tempdir().unwrap();
        assert!(matches!(
            unpack_context(tar.as_slice(), dest.path(), 10 * 1024),
            Err(BuildError::ContextTooLarge { .. })
        ));
    }

    #[test]
    fn unpack_context_stops_at_the_entry_cap() {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            for i in 0..=MAX_CONTEXT_ENTRIES {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("f{i}"), &b""[..])
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let dest = tempfile::tempdir().unwrap();
        assert!(matches!(
            unpack_context(archive.as_slice(), dest.path(), u64::MAX),
            Err(BuildError::ContextTooManyEntries { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unpack_context_strips_setuid_and_setgid_bits() {
        use std::os::unix::fs::PermissionsExt;
        let tar = tar_of(&[("tool", 0o6755, b"#!/bin/sh\n")]);
        let dest = tempfile::tempdir().unwrap();
        unpack_context(tar.as_slice(), dest.path(), 1024 * 1024).unwrap();
        let mode = std::fs::metadata(dest.path().join("tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7000, 0, "setuid/setgid/sticky bits must be gone");
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn context_download_url_at_targets_the_given_node() {
        let url = context_download_url_at("http", "10.0.0.7:5050", "sha256:abc");
        assert_eq!(
            url,
            "http://10.0.0.7:5050/v2/_buildcontext/blobs/sha256:abc"
        );
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
}

/// OCI runtime specification generation.
///
/// Generates a simplified OCI runtime spec from a `config::AppSpec`.
/// We define our own types rather than importing the full OCI spec
/// crate, because we only need a subset and want control over the
/// serialisation and derives.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
use crate::config::job::JobSpec;
use crate::config::types::{EnvValue, ResourceRange};
use crate::grill::cgroup;

/// The root a host workload (`exec` or `script`) runs in: the node's own
/// filesystem, not an image. An init container of a host app inherits it.
pub const HOST_ROOT: &str = "proc-grill:host";

/// The `oom_score_adj` every workload process starts with: apps, init
/// containers, jobs, task executors and the tasks they run.
///
/// The kernel's global OOM killer picks the process with the highest
/// score, roughly its share of memory in thousandths plus this value
/// (-1000 to 1000). The systemd units we ship run Bun at -900, and a child
/// inherits its parent's value, so without this every workload ran at -900
/// too and Bun was no safer than the workloads it supervises. At +500 a
/// workload goes before Bun, before the node's own daemons (which run at 0
/// or below) and before Lima's guest agent (-500), however small it is.
/// Losing a task or one replica costs far less than losing the node agent.
/// A workload's own cgroup `memory.max` still decides a cgroup OOM; this
/// only orders the node-wide one.
pub const WORKLOAD_OOM_SCORE_ADJ: i32 = 500;

/// A simplified OCI runtime specification.
///
/// Contains the fields Reliaburger actually uses. Additional fields
/// are added as later phases require them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciSpec {
    /// Internal owned PID-1 executor contract; never an application capability override.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reusable_executor: bool,
    /// Trusted execution discriminator generated from exec/script, never an image name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_process: bool,
    /// An init container ([`generate_init_oci_spec`]): it mounts the app's
    /// volumes but leaves their ownership with the app's user, so a runtime
    /// that hands a managed volume to its container's user skips it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub initialiser: bool,
    pub root: OciRoot,
    pub process: OciProcess,
    pub mounts: Vec<OciMount>,
    pub linux: OciLinux,
    /// Host-port publication for the workload, when the app declares a
    /// port. Not part of the OCI runtime spec proper — runtimes with
    /// per-container networking (runc) read it to install the DNAT map
    /// element alongside the network namespace. `None` is omitted from
    /// the serialised spec, so `#[serde(default)]` reads it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_mapping: Option<PortMapping>,
}

impl OciSpec {
    /// The same spec with every environment entry cut down to its variable
    /// name, so no value (decrypted secrets included) remains.
    ///
    /// A retired runtime intent keeps this copy instead of the original. A
    /// launched entry is always `NAME=value`, so a scrubbed entry (no `=`)
    /// can never be mistaken for a live one, and scrubbing twice changes
    /// nothing.
    pub fn without_environment_values(&self) -> OciSpec {
        let mut scrubbed = self.clone();
        for entry in &mut scrubbed.process.env {
            if let Some((name, _value)) = entry.split_once('=') {
                *entry = name.to_string();
            }
        }
        scrubbed
    }

    /// Whether `journal`, a runtime intent's copy, records this spec.
    ///
    /// A live intent keeps the exact spec, so it must be equal. A retired
    /// intent keeps [`OciSpec::without_environment_values`], so everything
    /// except environment values must be equal.
    pub fn matches_journal(&self, journal: &OciSpec) -> bool {
        self == journal || self.without_environment_values() == *journal
    }
}

/// A published port: traffic to `host_port` on the node reaches the
/// workload's `container_port` inside its network namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
}

/// The container's root filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciRoot {
    pub path: String,
    pub readonly: bool,
}

/// The container's main process configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciProcess {
    /// Optional POSIX limits enforced by OCI runtimes before executing the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<OciRlimit>,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: OciUser,
    /// Linux capability sets. `None` gives the process no capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<OciCapabilities>,
    /// The process's `oom_score_adj` (the OCI `oomScoreAdj` field, which runc
    /// applies). Workloads carry [`WORKLOAD_OOM_SCORE_ADJ`]. `None` keeps the
    /// score the process inherits, which suits Bun's own runtime commands.
    #[serde(
        rename = "oomScoreAdj",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub oom_score_adj: Option<i32>,
    /// What the workload asked for before its image config fills the gaps.
    ///
    /// Not part of the OCI runtime spec. `Some` means `args`, `env`, `cwd`
    /// and `user` are provisional: a runtime that unpacks the image itself
    /// (runc) resolves them against the image's `Entrypoint`, `Cmd`, `Env`,
    /// `WorkingDir` and `User` with Kubernetes rules, then drops this field
    /// before writing `config.json`. `None` means the process is final.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ProcessOverrides>,
}

/// One standard OCI process limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciRlimit {
    #[serde(rename = "type")]
    pub kind: String,
    pub hard: u64,
    pub soft: u64,
}

/// The parts of a container's process an app may set, each replacing one
/// piece of the image's own config (the Kubernetes rules).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOverrides {
    /// Replaces the image's `Entrypoint` (and drops its `Cmd`) when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Replaces the image's `Cmd` when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Replaces the image's `WorkingDir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Replaces the uid from the image's `User`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<u32>,
    /// Replaces the gid from the image's `User`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
}

/// Linux capability sets for the container process, by OCI capability name
/// (`CAP_CHOWN` and so on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OciCapabilities {
    pub bounding: Vec<String>,
    pub effective: Vec<String>,
    pub permitted: Vec<String>,
}

/// The user and group to run the container process as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciUser {
    pub uid: u32,
    pub gid: u32,
}

/// A filesystem mount inside the container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciMount {
    pub destination: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub mount_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// Linux-specific container configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciLinux {
    pub namespaces: Vec<OciNamespace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<OciResources>,
    /// Absolute path within the cgroup hierarchy, excluding its host mount point.
    #[serde(rename = "cgroupsPath", skip_serializing_if = "Option::is_none")]
    pub cgroups_path: Option<String>,
    #[serde(rename = "uidMappings", skip_serializing_if = "Option::is_none")]
    pub uid_mappings: Option<Vec<OciIdMapping>>,
    #[serde(rename = "gidMappings", skip_serializing_if = "Option::is_none")]
    pub gid_mappings: Option<Vec<OciIdMapping>>,
}

impl OciLinux {
    /// Resolve an explicit OCI hierarchy path to its host cgroup v2 directory.
    /// Relative, root, non-canonical and legacy host-prefixed paths refuse:
    /// none gives Reliaburger an unambiguous workload identity.
    pub fn host_cgroup_path(&self) -> Option<PathBuf> {
        self.cgroup_path_under(Path::new("/sys/fs/cgroup"))
    }

    /// [`Self::host_cgroup_path`] under a cgroup v2 mount other than
    /// `/sys/fs/cgroup`, so tests can stand a directory in for it.
    pub fn cgroup_path_under(&self, mount: &Path) -> Option<PathBuf> {
        let path = self.cgroups_path.as_deref()?;
        let relative = path.strip_prefix('/')?;
        if relative.is_empty()
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || Path::new(path).starts_with("/sys/fs/cgroup")
        {
            return None;
        }
        Some(mount.join(relative))
    }
}

/// The OCI `cgroupsPath` for a host cgroup directory. The generators receive
/// the host path Bun uses for policy and resource files, while OCI resolves
/// an absolute path from the cgroup mount point.
pub fn hierarchy_cgroup_path(host_path: &str) -> String {
    match host_path.strip_prefix("/sys/fs/cgroup/") {
        Some(relative) => format!("/{relative}"),
        None => host_path.to_owned(),
    }
}

/// UID/GID mapping for user namespaces.
///
/// Maps a range of IDs inside the container to a range on the host.
/// Used by rootless runc to map the current user to container root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciIdMapping {
    #[serde(rename = "containerID")]
    pub container_id: u32,
    #[serde(rename = "hostID")]
    pub host_id: u32,
    pub size: u32,
}

/// A Linux namespace to create for the container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciNamespace {
    #[serde(rename = "type")]
    pub ns_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Resource limits for the container.
///
/// The `cpu`/`memory` blocks carry the *hard* limits (quota and OOM
/// ceiling). Resource *requests* have no dedicated field in the classic
/// OCI schema, so they ride in `unified` — see that field's doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciResources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<OciCpuResources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<OciMemoryResources>,
    /// The process and thread ceiling (`pids.max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<OciPidsResources>,
    /// cgroup v2 native controller keys, written verbatim by runc into
    /// the workload's cgroup. We use it for the resource *requests* that
    /// the classic `cpu`/`memory` blocks can't express:
    ///
    /// - `cpu.weight` (1-10000): proportional CPU share under contention,
    ///   derived from the CPU request. Passing it here avoids runc's
    ///   lossy `cpu.shares` (2-262144) -> weight conversion, since our
    ///   value is already in the v2 range.
    /// - `memory.high` (bytes): the soft limit. The kernel throttles and
    ///   reclaims a workload that crosses it, before the hard
    ///   `memory.max` triggers an OOM kill. There is no classic OCI field
    ///   for it (`memory.reservation` maps to `memory.low`, a protection
    ///   floor with the opposite meaning).
    ///
    /// `BTreeMap` keeps serialisation deterministic; empty maps are
    /// omitted so specs without requests are unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unified: BTreeMap<String, String>,
}

/// CPU resource limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciCpuResources {
    pub quota: i64,
    pub period: u64,
}

/// Memory resource limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciMemoryResources {
    pub limit: i64,
}

/// Process-count limit: runc writes `limit` to the cgroup's `pids.max`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciPidsResources {
    pub limit: i64,
}

/// Generate an OCI runtime spec from a config AppSpec.
///
/// Environment variables with `EnvValue::Encrypted` are passed through
/// as the literal encrypted string. To decrypt them, use
/// [`generate_oci_spec_with_decryptor`] with a `SecretDecryptor`.
#[allow(clippy::too_many_arguments)]
pub fn generate_oci_spec(
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
    instance_id: &str,
    host_port: Option<u16>,
    cgroup_path: &str,
    volumes_dir: Option<&Path>,
    netns_path: Option<&str>,
) -> OciSpec {
    generate_oci_spec_with_decryptor(
        app_name,
        namespace,
        spec,
        instance_id,
        host_port,
        cgroup_path,
        volumes_dir,
        netns_path,
        None,
    )
    // Infallible with no decryptor: encrypted values are passed through, so the
    // env build never hits a decryption failure.
    .expect("env build without a decryptor cannot fail")
}

/// Generate an OCI runtime spec, decrypting `ENC[AGE:...]` env values with the
/// supplied `decryptor` (see [`SecretDecryptor`]).
///
/// When `decryptor` is `None`, encrypted values are passed through unchanged —
/// the caller is responsible for refusing to start a workload whose secrets it
/// cannot decrypt (the agent fails such deploys closed rather than leaking
/// ciphertext into the container environment).
#[allow(clippy::too_many_arguments)]
pub fn generate_oci_spec_with_decryptor(
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
    instance_id: &str,
    host_port: Option<u16>,
    cgroup_path: &str,
    volumes_dir: Option<&Path>,
    netns_path: Option<&str>,
    decryptor: Option<&SecretDecryptor>,
) -> Result<OciSpec, String> {
    let env = build_env_with_decryptor(spec, decryptor)?;
    let args = build_args(app_name, spec);
    let mounts = build_mounts(
        spec,
        host_port,
        app_name,
        namespace,
        instance_id,
        volumes_dir,
    );

    let namespaces = standard_namespaces(netns_path);

    let resources = build_resources(spec);

    Ok(OciSpec {
        reusable_executor: false,
        host_process: spec.exec.is_some() || spec.script.is_some(),
        initialiser: false,
        root: OciRoot {
            // Process workloads (exec/script) use the host root filesystem.
            // Container workloads use the image reference (Apple Container)
            // or resolved rootfs path (runc).
            path: if spec.exec.is_some() || spec.script.is_some() {
                HOST_ROOT.to_string()
            } else {
                spec.image.clone().unwrap_or_else(|| {
                    format!("/var/lib/reliaburger/images/{namespace}/{app_name}/rootfs")
                })
            },
            readonly: false,
        },
        process: OciProcess {
            rlimits: Vec::new(),
            args,
            env,
            cwd: spec
                .working_dir
                .as_ref()
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string()),
            // Runtimes that can't read the image config (the process grill,
            // Apple Container's CLI) run as nobody (65534). Runc replaces
            // this with the image's `User`, inside a user namespace.
            user: OciUser {
                uid: spec.run_as_user.unwrap_or(65534),
                gid: spec.run_as_group.unwrap_or(65534),
            },
            capabilities: None,
            oom_score_adj: Some(WORKLOAD_OOM_SCORE_ADJ),
            overrides: app_overrides(spec),
        },
        mounts,
        linux: OciLinux {
            namespaces,
            resources,
            cgroups_path: Some(hierarchy_cgroup_path(cgroup_path)),
            uid_mappings: None,
            gid_mappings: None,
        },
        port_mapping: host_port.zip(spec.port).map(|(hp, cp)| PortMapping {
            host_port: hp,
            container_port: cp,
        }),
    })
}

/// A callback for decrypting `ENC[AGE:...]` values.
///
/// Accepts the encrypted string (including the `ENC[AGE:...]` wrapper)
/// and returns the decrypted plaintext. If decryption fails, the error
/// message is used instead (prefixed with `DECRYPT_ERROR:`).
pub type SecretDecryptor = Box<dyn Fn(&str) -> Result<String, String>>;

/// Build env vars with optional secret decryption.
///
/// When a `SecretDecryptor` is provided, `ENC[AGE:...]` values are decrypted
/// before injection. A decryption **failure** is an error (M4): the container
/// must not start with a broken secret — the old code injected
/// `KEY=DECRYPT_ERROR:...` and ran anyway. Without a decryptor the values are
/// passed through as literal strings (pre-Phase 4 behaviour); the caller is
/// responsible for refusing to start a workload whose secrets it can't decrypt.
pub fn build_env_with_decryptor(
    spec: &AppSpec,
    decryptor: Option<&SecretDecryptor>,
) -> Result<Vec<String>, String> {
    let mut env = Vec::new();
    for (key, value) in &spec.env {
        match value {
            EnvValue::Plain(v) => env.push(format!("{key}={v}")),
            EnvValue::Encrypted(v) => {
                if let Some(decrypt) = decryptor {
                    let plaintext =
                        decrypt(v).map_err(|e| format!("failed to decrypt secret {key:?}: {e}"))?;
                    env.push(format!("{key}={plaintext}"));
                } else {
                    // No decryptor available — pass through as-is.
                    env.push(format!("{key}={v}"));
                }
            }
        }
    }
    Ok(env)
}

/// Build the process arguments from an app spec.
///
/// Returns the app's `command` followed by its `args`. When both are
/// empty, runc resolves the image's `Entrypoint` and `Cmd` from
/// [`OciProcess::overrides`]. ProcessGrill runs no images (#914).
fn build_args(app_name: &str, spec: &AppSpec) -> Vec<String> {
    let _ = app_name;

    // Process workloads: exec binary or inline script via /bin/sh
    if let Some(ref exec_path) = spec.exec {
        let mut args = vec![exec_path.to_string_lossy().to_string()];
        args.extend(spec.command.iter().cloned());
        return args;
    }
    if let Some(ref script) = spec.script {
        return vec!["/bin/sh".to_string(), "-c".to_string(), script.clone()];
    }

    spec.command.iter().chain(&spec.args).cloned().collect()
}

/// What an image-based app overrides in its image's config.
///
/// `None` for process workloads and for apps without an image: there is no
/// image config for their process to be resolved against.
fn app_overrides(spec: &AppSpec) -> Option<ProcessOverrides> {
    if spec.image.is_none() || spec.exec.is_some() || spec.script.is_some() {
        return None;
    }
    Some(ProcessOverrides {
        command: spec.command.clone(),
        args: spec.args.clone(),
        working_dir: spec
            .working_dir
            .as_ref()
            .map(|dir| dir.to_string_lossy().into_owned()),
        user: spec.run_as_user,
        group: spec.run_as_group,
    })
}

/// Where an instance's inline config files live on the node:
/// `{volumes_dir}/.config/{namespace}/{app}/{instance_id}`. Bun removes the
/// directory when it retires the instance.
pub fn inline_config_dir(
    volumes_dir: &Path,
    namespace: &str,
    app_name: &str,
    instance_id: &str,
) -> PathBuf {
    volumes_dir
        .join(".config")
        .join(namespace)
        .join(app_name)
        .join(instance_id)
}

/// The host file name for the inline config file mounted at `container_path`:
/// a hash of the whole path, so files with the same basename stay apart and
/// no container path can name anything outside the instance's directory.
fn inline_config_file_name(container_path: &Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(container_path.as_os_str().as_encoded_bytes());
    hex::encode(&digest[..8])
}

/// Write `content` to `path` by renaming a fresh file over it, so an entry
/// already at `path` (a symlink, say) is replaced rather than written through.
fn write_inline_config(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut staged = path.as_os_str().to_owned();
    staged.push(format!(".{}.tmp", std::process::id()));
    let staged = PathBuf::from(staged);
    let _ = std::fs::remove_file(&staged);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    file.write_all(content.as_bytes())?;
    drop(file);
    std::fs::rename(&staged, path)
}

fn build_mounts(
    spec: &AppSpec,
    _host_port: Option<u16>,
    app_name: &str,
    namespace: &str,
    instance_id: &str,
    volumes_dir: Option<&Path>,
) -> Vec<OciMount> {
    let mut mounts = standard_mounts();

    // Config files: read-only bind mounts.
    // Inline content is written to a file under volumes_dir so it can be
    // bind-mounted into the container. Source paths are used directly.
    // Each instance gets its own copy, named after its container path, so
    // two files with one basename can't collide and a rollout never
    // rewrites the file under a running instance (#701).
    for cf in &spec.config_file {
        let source = if let Some(ref source_path) = cf.source {
            Some(PathBuf::from(source_path))
        } else if let Some(ref content) = cf.content {
            let base = volumes_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/var/lib/reliaburger/volumes"));
            let config_dir = inline_config_dir(&base, namespace, app_name, instance_id);
            let host_path = config_dir.join(inline_config_file_name(&cf.path));
            if let Err(e) = std::fs::create_dir_all(&config_dir) {
                eprintln!("failed to create config dir {}: {e}", config_dir.display());
            } else if let Err(e) = write_inline_config(&host_path, content) {
                eprintln!("failed to write inline config {}: {e}", host_path.display());
            }
            Some(host_path)
        } else {
            None
        };

        mounts.push(OciMount {
            destination: cf.path.clone(),
            source,
            mount_type: Some("bind".to_string()),
            options: vec!["bind".to_string(), "ro".to_string()],
        });
    }

    // Volumes: read-write bind mounts
    for vol in &spec.volumes {
        let host_path = if let Some(source) = &vol.source {
            // HostPath mode: use the explicit host path
            source.clone()
        } else {
            // Managed mode: resolve to a subdirectory under volumes_dir
            let base = volumes_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/var/lib/reliaburger/volumes"));
            base.join(namespace)
                .join(app_name)
                .join(vol.path.strip_prefix("/").unwrap_or(&vol.path))
        };

        mounts.push(OciMount {
            destination: vol.path.clone(),
            source: Some(host_path),
            mount_type: Some("bind".to_string()),
            options: vec!["bind".to_string(), "rw".to_string()],
        });
    }

    // Workload identity mount — populated by Bun after CSR signing.
    // Starts empty; cert.pem, key.pem, ca.pem, bundle.pem, token, and
    // meta.json appear once the council signs the workload's CSR. The
    // source is per-INSTANCE (PKI7): replicas of the same app must not
    // share (or overwrite) each other's private key. Bun prepares the
    // directory (tmpfs-backed on Linux root) before create; the
    // create_dir_all here is a fallback for direct grill users.
    let identity_host_dir = crate::sesame::identity::instance_identity_dir(
        &volumes_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/var/lib/reliaburger/volumes")),
        instance_id,
    );
    let _ = std::fs::create_dir_all(&identity_host_dir);
    mounts.push(OciMount {
        destination: PathBuf::from("/run/reliaburger/identity"),
        source: Some(identity_host_dir),
        mount_type: Some("bind".to_string()),
        options: vec!["bind".to_string(), "ro".to_string()],
    });

    mounts
}

/// Standard Linux namespaces for container isolation.
///
/// If `netns_path` is provided, the container joins that pre-created
/// network namespace (where the veth pair is already configured)
/// instead of creating a new empty one.
pub fn standard_namespaces(netns_path: Option<&str>) -> Vec<OciNamespace> {
    vec![
        OciNamespace {
            ns_type: "pid".to_string(),
            path: None,
        },
        OciNamespace {
            ns_type: "ipc".to_string(),
            path: None,
        },
        OciNamespace {
            ns_type: "uts".to_string(),
            path: None,
        },
        OciNamespace {
            ns_type: "mount".to_string(),
            path: None,
        },
        OciNamespace {
            ns_type: "network".to_string(),
            path: netns_path.map(String::from),
        },
    ]
}

/// Standard base mounts (/proc, /dev, /sys) for OCI containers.
pub fn standard_mounts() -> Vec<OciMount> {
    vec![
        OciMount {
            destination: PathBuf::from("/proc"),
            source: Some(PathBuf::from("proc")),
            mount_type: Some("proc".to_string()),
            options: vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
            ],
        },
        OciMount {
            destination: PathBuf::from("/dev"),
            source: Some(PathBuf::from("tmpfs")),
            mount_type: Some("tmpfs".to_string()),
            options: vec![
                "nosuid".to_string(),
                "strictatime".to_string(),
                "mode=755".to_string(),
                "size=65536k".to_string(),
            ],
        },
        OciMount {
            destination: PathBuf::from("/sys"),
            source: Some(PathBuf::from("sysfs")),
            mount_type: Some("sysfs".to_string()),
            options: vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "ro".to_string(),
            ],
        },
    ]
}

fn build_resources(spec: &AppSpec) -> Option<OciResources> {
    build_resources_from_ranges(spec.cpu.as_ref(), spec.memory.as_ref())
}

/// Build the OCI resource block from CPU and memory ranges.
///
/// Hard limits go into the `cpu`/`memory` blocks; requests go into the
/// `unified` map as `cpu.weight` and `memory.high`. A request is only
/// emitted when it is actually declared (`request > 0`) — an app that
/// sets only a limit (e.g. `cpu = "0-1000m"`) gets the ceiling without a
/// weight or soft limit forced on it.
fn build_resources_from_ranges(
    cpu: Option<&ResourceRange>,
    memory: Option<&ResourceRange>,
) -> Option<OciResources> {
    let mut unified = BTreeMap::new();

    let cpu_res = cpu.map(|range| {
        if range.request > 0 {
            let weight = cgroup::cpu_weight_from_millicores(range.request);
            unified.insert("cpu.weight".to_string(), weight.to_string());
        }
        OciCpuResources {
            quota: (range.limit * 100_000 / 1000) as i64,
            period: 100_000,
        }
    });

    let memory_res = memory.map(|range| {
        if range.request > 0 {
            unified.insert("memory.high".to_string(), range.request.to_string());
        }
        OciMemoryResources {
            limit: range.limit as i64,
        }
    });

    if cpu_res.is_some() || memory_res.is_some() {
        Some(OciResources {
            pids: None,
            cpu: cpu_res,
            memory: memory_res,
            unified,
        })
    } else {
        None
    }
}

/// Generate an OCI runtime spec from a job spec.
///
/// Jobs are simpler than apps: no port allocation, no health checks,
/// no config files or volumes. The process runs to completion.
pub fn generate_job_oci_spec(
    job_name: &str,
    namespace: &str,
    spec: &JobSpec,
    cgroup_path: &str,
    netns_path: Option<&str>,
) -> OciSpec {
    let env: Vec<String> = spec
        .env
        .iter()
        .map(|(key, value)| match value {
            EnvValue::Plain(v) => format!("{key}={v}"),
            EnvValue::Encrypted(v) => format!("{key}={v}"),
        })
        .collect();

    // Process workloads: exec binary or inline script via /bin/sh
    let args = if let Some(ref exec_path) = spec.exec {
        let mut a = vec![exec_path.to_string_lossy().to_string()];
        a.extend(spec.command.clone().unwrap_or_default());
        a
    } else if let Some(ref script) = spec.script {
        vec!["/bin/sh".to_string(), "-c".to_string(), script.clone()]
    } else {
        spec.command.clone().unwrap_or_default()
    };

    let mut resources = build_resources_from_ranges(spec.cpu.as_ref(), spec.memory.as_ref());
    if spec.image.is_some()
        && spec.exec.is_none()
        && spec.script.is_none()
        && spec.memory.is_some()
        && let Some(resources) = &mut resources
    {
        // A command's memory budget must not turn into unaccounted swap I/O.
        // Use the same contract for fresh and reused image execution.
        resources
            .unified
            .insert("memory.swap.max".into(), "0".into());
    }
    if let Some(limit) = spec.pids {
        // Only a reused executor's commands get a default; a fresh container
        // without `pids` keeps no limit, like one without `cpu` or `memory`.
        resources
            .get_or_insert_with(|| OciResources {
                cpu: None,
                memory: None,
                pids: None,
                unified: BTreeMap::new(),
            })
            .pids = Some(OciPidsResources {
            limit: i64::from(limit.get()),
        });
    }

    OciSpec {
        reusable_executor: false,
        host_process: spec.is_host(),
        initialiser: false,
        root: OciRoot {
            path: if spec.is_host() {
                HOST_ROOT.to_string()
            } else {
                spec.image.clone().unwrap_or_else(|| {
                    format!("/var/lib/reliaburger/images/{namespace}/{job_name}/rootfs")
                })
            },
            readonly: false,
        },
        process: OciProcess {
            rlimits: Vec::new(),
            args,
            env,
            cwd: "/".to_string(),
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
            capabilities: None,
            oom_score_adj: Some(WORKLOAD_OOM_SCORE_ADJ),
            overrides: (spec.image.is_some() && spec.exec.is_none() && spec.script.is_none()).then(
                || ProcessOverrides {
                    command: spec.command.clone().unwrap_or_default(),
                    ..ProcessOverrides::default()
                },
            ),
        },
        mounts: standard_mounts(),
        linux: OciLinux {
            namespaces: standard_namespaces(netns_path),
            resources,
            cgroups_path: Some(hierarchy_cgroup_path(cgroup_path)),
            uid_mappings: None,
            gid_mappings: None,
        },
        // Jobs run to completion and don't publish ports.
        port_mapping: None,
    }
}

/// Generate the OCI spec for an init container of an app whose own spec is
/// `app`.
///
/// The init runs `init.command` in its own image, or the app's when it names
/// none (on the host, for an `exec` or `script` app), with the app's environment (secrets already decrypted), the app's
/// mounts (volumes, config files and identity, host paths already approved)
/// and the app's cgroup. It takes nothing from the app's process settings and
/// publishes no port. Building on the app's finished spec rather than its
/// config means the init can't drift from what the app itself got.
pub fn generate_init_oci_spec(
    init: &crate::config::app::InitContainerSpec,
    app: &OciSpec,
) -> OciSpec {
    let root = init.image.clone().unwrap_or_else(|| app.root.path.clone());
    // An image's config fills in what the init doesn't set, as it does for
    // the app; a bare root filesystem has no config to read.
    let from_image = init.image.is_some() || app.process.overrides.is_some();
    OciSpec {
        reusable_executor: false,
        // A host app's init that names no image runs on the host like its
        // app, so the mixed runtime routes it there too (#935).
        host_process: app.host_process && init.image.is_none(),
        initialiser: true,
        root: OciRoot {
            path: root,
            readonly: false,
        },
        process: OciProcess {
            rlimits: Vec::new(),
            args: init.command.clone(),
            env: app.process.env.clone(),
            cwd: "/".to_string(),
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
            capabilities: None,
            oom_score_adj: Some(WORKLOAD_OOM_SCORE_ADJ),
            overrides: from_image.then(|| ProcessOverrides {
                command: init.command.clone(),
                ..ProcessOverrides::default()
            }),
        },
        mounts: app.mounts.clone(),
        linux: OciLinux {
            namespaces: standard_namespaces(None),
            resources: None,
            cgroups_path: app.linux.cgroups_path.clone(),
            uid_mappings: None,
            gid_mappings: None,
        },
        // Init containers run before the app and don't publish ports.
        port_mapping: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ConfigFileSpec, VolumeSpec};

    fn minimal_app() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn init_container(
        image: Option<&str>,
        command: &[&str],
    ) -> crate::config::app::InitContainerSpec {
        crate::config::app::InitContainerSpec {
            image: image.map(String::from),
            command: command.iter().map(|part| part.to_string()).collect(),
        }
    }

    /// The spec Bun would generate for instance `default__web-0` of `app`.
    fn app_oci(app: &AppSpec) -> OciSpec {
        generate_oci_spec(
            "web",
            "default",
            app,
            "default__web-0",
            None,
            "/cg",
            None,
            None,
        )
    }

    #[test]
    fn init_spec_uses_its_own_image() {
        // #701: `[[app.api.init]] image = "migrate:v2"` used to run its
        // command in the app's image.
        let app: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        let app = app_oci(&app);
        let init = generate_init_oci_spec(&init_container(Some("migrate:v2"), &["migrate"]), &app);
        assert_eq!(init.root.path, "migrate:v2");
        assert_eq!(init.process.args, vec!["migrate".to_string()]);
        assert_eq!(
            init.process.overrides.unwrap().command,
            vec!["migrate".to_string()]
        );

        // Without an image of its own it inherits the app's.
        let inherited = generate_init_oci_spec(&init_container(None, &["true"]), &app);
        assert_eq!(inherited.root.path, "myapp:v1");
    }

    /// #935: an `exec` or `script` app's init container that names no image
    /// runs on the host like its app, so it records as a host spec. One
    /// that names an image, or an image app's init, stays a container.
    #[test]
    fn an_init_container_of_a_host_app_records_as_a_host_spec() {
        for host_app in [r#"exec = "/bin/sleep""#, r#"script = "sleep 60""#] {
            let app = app_oci(&toml::from_str(host_app).unwrap());
            assert!(app.host_process, "{host_app}");

            let init = generate_init_oci_spec(&init_container(None, &["/bin/true"]), &app);
            assert!(init.host_process, "{host_app}: init must be a host spec");
            assert_eq!(init.root.path, HOST_ROOT);

            let own_image =
                generate_init_oci_spec(&init_container(Some("migrate:v2"), &["migrate"]), &app);
            assert!(
                !own_image.host_process,
                "{host_app}: an init with its own image is a container"
            );
        }

        let image_app = app_oci(&minimal_app());
        let init = generate_init_oci_spec(&init_container(None, &["true"]), &image_app);
        assert!(!init.host_process, "an image app's init is a container");
    }

    #[test]
    fn init_spec_gets_app_env_and_volume_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        let app: AppSpec = toml::from_str(
            r#"
            image = "myapp:v1"
            env = { DATABASE_URL = "postgres://db/app" }

            [[volumes]]
            path = "/data"

            [[config_file]]
            path = "/etc/myapp/app.conf"
            content = "key = value"
            "#,
        )
        .unwrap();
        let app = generate_oci_spec(
            "web",
            "default",
            &app,
            "default__web-0",
            None,
            "/cg",
            Some(tmp.path()),
            None,
        );
        let init = generate_init_oci_spec(&init_container(Some("migrate:v2"), &["migrate"]), &app);

        assert_eq!(init.process.env, app.process.env);
        assert!(
            init.process
                .env
                .contains(&"DATABASE_URL=postgres://db/app".to_string()),
            "{:?}",
            init.process.env
        );
        for destination in ["/data", "/etc/myapp/app.conf"] {
            let app_mount = app
                .mounts
                .iter()
                .find(|m| m.destination == Path::new(destination))
                .unwrap();
            assert!(
                init.mounts.contains(app_mount),
                "init is missing the app's {destination} mount: {:?}",
                init.mounts
            );
        }
        assert_eq!(init.linux.cgroups_path, app.linux.cgroups_path);
        // The app owns its volumes; an init borrowing them must not take
        // their ownership away from the app's user.
        assert!(init.initialiser && !app.initialiser);
        assert_eq!(init.port_mapping, None);
    }

    #[test]
    fn inline_config_files_with_same_basename_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let app: AppSpec = toml::from_str(
            r#"
            image = "myapp:v1"

            [[config_file]]
            path = "/etc/a/config.yaml"
            content = "a"

            [[config_file]]
            path = "/etc/b/config.yaml"
            content = "b"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &app,
            "default__web-0",
            None,
            "/cg",
            Some(tmp.path()),
            None,
        );
        let content = |destination: &str| {
            let mount = oci
                .mounts
                .iter()
                .find(|m| m.destination == Path::new(destination))
                .unwrap();
            std::fs::read_to_string(mount.source.as_ref().unwrap()).unwrap()
        };
        assert_eq!(content("/etc/a/config.yaml"), "a");
        assert_eq!(content("/etc/b/config.yaml"), "b");
    }

    #[test]
    fn inline_config_files_are_kept_per_instance() {
        // A rollout writes the new content for the new instance only: the
        // old instance keeps reading the file it started with (#701).
        let tmp = tempfile::tempdir().unwrap();
        let app = |content: &str| -> AppSpec {
            toml::from_str(&format!(
                "image = \"myapp:v1\"\n[[config_file]]\npath = \"/etc/app.conf\"\ncontent = {content:?}\n"
            ))
            .unwrap()
        };
        let source = |instance: &str, content: &str| {
            let oci = generate_oci_spec(
                "web",
                "default",
                &app(content),
                instance,
                None,
                "/cg",
                Some(tmp.path()),
                None,
            );
            oci.mounts
                .iter()
                .find(|m| m.destination == Path::new("/etc/app.conf"))
                .and_then(|m| m.source.clone())
                .unwrap()
        };
        let old = source("default__web-0", "old");
        let new = source("default__web-g1-0", "new");
        assert_ne!(old, new);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "new");
        assert_eq!(
            old.parent().unwrap(),
            inline_config_dir(tmp.path(), "default", "web", "default__web-0")
        );
    }

    #[test]
    fn generate_minimal_app() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        assert_eq!(oci.root.path, "test:v1");
        assert_eq!(oci.process.cwd, "/");
        assert_eq!(oci.process.user.uid, 65534);
        assert!(oci.process.env.is_empty());
    }

    #[test]
    fn port_mapping_set_when_app_declares_a_port() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            port = 8080
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            Some(30017),
            "/cg",
            None,
            None,
        );

        assert_eq!(
            oci.port_mapping,
            Some(PortMapping {
                host_port: 30017,
                container_port: 8080,
            })
        );
    }

    #[test]
    fn port_mapping_absent_without_allocated_host_port() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            port = 8080
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec("web", "default", &spec, "web-0", None, "/cg", None, None);

        assert_eq!(oci.port_mapping, None);
    }

    #[test]
    fn port_mapping_absent_when_app_has_no_port() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            Some(30017),
            "/cg",
            None,
            None,
        );

        assert_eq!(oci.port_mapping, None);
    }

    #[test]
    fn port_mapping_survives_record_round_trip() {
        // New records carry the mapping through serde…
        let spec: AppSpec = toml::from_str(r#"image = "t:v1""#).unwrap();
        let mut oci = generate_oci_spec("web", "default", &spec, "web-0", None, "/cg", None, None);
        oci.port_mapping = Some(PortMapping {
            host_port: 30017,
            container_port: 8080,
        });
        let json = serde_json::to_string(&oci).unwrap();
        let back: OciSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.port_mapping, oci.port_mapping);

        // …and so does its absence, which serde omits from the record.
        oci.port_mapping = None;
        let json = serde_json::to_string(&oci).unwrap();
        assert!(!json.contains("port_mapping"));
        let back: OciSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.port_mapping, None);
    }

    fn spec_for(toml_source: &str) -> OciSpec {
        let app: AppSpec = toml::from_str(toml_source).unwrap();
        generate_oci_spec("web", "default", &app, "web-0", None, "/cg", None, None)
    }

    #[test]
    fn image_apps_carry_their_overrides_for_the_runtime() {
        let oci = spec_for(
            r#"
            image = "nginx:1"
            command = ["nginx"]
            args = ["-g", "daemon off;"]
            working_dir = "/srv"
            run_as_user = 101
            "#,
        );
        assert_eq!(
            oci.process.overrides,
            Some(ProcessOverrides {
                command: vec!["nginx".into()],
                args: vec!["-g".into(), "daemon off;".into()],
                working_dir: Some("/srv".into()),
                user: Some(101),
                group: None,
            })
        );
        // Runtimes that can't read the image config still see one argv.
        assert_eq!(oci.process.args, ["nginx", "-g", "daemon off;"]);
        assert_eq!(oci.process.cwd, "/srv");
        assert_eq!(oci.process.user.uid, 101);
    }

    #[test]
    fn an_image_app_without_command_leaves_the_process_to_its_image() {
        let oci = spec_for(r#"image = "redis:8""#);
        assert!(oci.process.args.is_empty());
        assert_eq!(oci.process.overrides, Some(ProcessOverrides::default()));
    }

    #[test]
    fn process_workloads_have_no_image_overrides() {
        let oci = spec_for(r#"script = "echo hi""#);
        assert_eq!(oci.process.overrides, None);
        let oci = spec_for(r#"command = ["sleep", "1"]"#);
        assert_eq!(oci.process.overrides, None);
    }

    #[test]
    fn init_containers_and_jobs_resolve_against_their_image_too() {
        let app: AppSpec = toml::from_str(r#"image = "web:v1""#).unwrap();
        let init = generate_init_oci_spec(&init_container(None, &["migrate"]), &app_oci(&app));
        assert_eq!(
            init.process.overrides.unwrap().command,
            vec!["migrate".to_string()]
        );
        let job: JobSpec = toml::from_str(r#"image = "tool:v1""#).unwrap();
        let job = generate_job_oci_spec("tool", "default", &job, "/cg", None);
        assert_eq!(job.process.overrides, Some(ProcessOverrides::default()));
    }

    #[test]
    fn generate_without_image_uses_filesystem_path() {
        let spec: AppSpec = toml::from_str(r#"command = ["echo", "hi"]"#).unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        assert_eq!(
            oci.root.path,
            "/var/lib/reliaburger/images/default/web/rootfs"
        );
    }

    #[test]
    fn generate_with_env_vars() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            FOO = "bar"
            BAZ = "qux"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        assert!(oci.process.env.contains(&"FOO=bar".to_string()));
        assert!(oci.process.env.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn generate_encrypted_env_passed_through() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            SECRET = "ENC[AGE:abc123]"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        assert!(
            oci.process
                .env
                .contains(&"SECRET=ENC[AGE:abc123]".to_string())
        );
    }

    #[test]
    fn generate_with_decryptor_decrypts_env() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            SECRET = "ENC[AGE:abc123]"
            PLAIN = "visible"
            "#,
        )
        .unwrap();
        let decryptor: SecretDecryptor = Box::new(|_enc: &str| Ok("plaintext".to_string()));
        let oci = generate_oci_spec_with_decryptor(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
            Some(&decryptor),
        )
        .unwrap();

        assert!(oci.process.env.contains(&"SECRET=plaintext".to_string()));
        assert!(oci.process.env.contains(&"PLAIN=visible".to_string()));
        assert!(
            !oci.process.env.iter().any(|e| e.contains("ENC[AGE:")),
            "ciphertext leaked into container env"
        );
    }

    #[test]
    fn generate_with_cpu_limits() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            cpu = "500m-1000m"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let resources = oci.linux.resources.unwrap();
        let cpu = resources.cpu.unwrap();
        assert_eq!(cpu.quota, 100_000); // 1000m = full CPU
        assert_eq!(cpu.period, 100_000);
    }

    #[test]
    fn generate_with_memory_limits() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            memory = "128Mi-512Mi"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let resources = oci.linux.resources.unwrap();
        let memory = resources.memory.unwrap();
        assert_eq!(memory.limit, 512 * 1024 * 1024);
    }

    #[test]
    fn cpu_request_produces_weight_in_unified() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            cpu = "500m-1000m"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let resources = oci.linux.resources.unwrap();
        // Hard limit unchanged: 1000m = a full CPU.
        assert_eq!(resources.cpu.unwrap().quota, 100_000);
        // Request of 500m maps to weight 50 (500 / 10), in the v2 range.
        assert_eq!(resources.unified.get("cpu.weight"), Some(&"50".to_string()));
    }

    #[test]
    fn memory_request_produces_high_in_unified() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            memory = "128Mi-512Mi"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let resources = oci.linux.resources.unwrap();
        // Hard ceiling stays the limit; the request becomes the soft limit.
        assert_eq!(resources.memory.unwrap().limit, 512 * 1024 * 1024);
        assert_eq!(
            resources.unified.get("memory.high"),
            Some(&(128 * 1024 * 1024).to_string())
        );
    }

    #[test]
    fn limit_only_does_not_force_weight_or_soft_limit() {
        // A leading `0-` request means "cap it here, but request nothing".
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            cpu = "0-1000m"
            memory = "0-512Mi"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let resources = oci.linux.resources.unwrap();
        // Hard limits still applied…
        assert_eq!(resources.cpu.unwrap().quota, 100_000);
        assert_eq!(resources.memory.unwrap().limit, 512 * 1024 * 1024);
        // …but no request-derived enforcement is forced on the workload.
        assert!(resources.unified.is_empty());
    }

    #[test]
    fn unified_map_omitted_from_json_when_empty() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            cpu = "0-1000m"
            "#,
        )
        .unwrap();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let json = serde_json::to_string(&oci).unwrap();
        assert!(!json.contains("unified"));
    }

    #[test]
    fn generate_without_resources_has_no_resources_block() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );
        assert!(oci.linux.resources.is_none());
    }

    #[test]
    fn generate_has_all_namespaces() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let ns_types: Vec<&str> = oci
            .linux
            .namespaces
            .iter()
            .map(|n| n.ns_type.as_str())
            .collect();
        assert!(ns_types.contains(&"pid"));
        assert!(ns_types.contains(&"ipc"));
        assert!(ns_types.contains(&"uts"));
        assert!(ns_types.contains(&"mount"));
        assert!(ns_types.contains(&"network"));
    }

    #[test]
    fn generate_sets_cgroups_path() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/sys/fs/cgroup/reliaburger/default/web/0",
            None,
            None,
        );

        assert_eq!(
            oci.linux.cgroups_path,
            Some("/reliaburger/default/web/0".to_string())
        );
    }

    #[test]
    fn generate_has_standard_mounts() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let mount_paths: Vec<&str> = oci
            .mounts
            .iter()
            .map(|m| m.destination.to_str().unwrap())
            .collect();
        assert!(mount_paths.contains(&"/proc"));
        assert!(mount_paths.contains(&"/dev"));
        assert!(mount_paths.contains(&"/sys"));
    }

    /// PKI7: the identity mount source is keyed by instance, so two
    /// replicas of the same app never share (or overwrite) key material.
    #[test]
    fn identity_mount_source_is_per_instance_not_per_app() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = minimal_app();

        let identity_source = |instance_id: &str| {
            let oci = generate_oci_spec(
                "web",
                "default",
                &spec,
                instance_id,
                None,
                "/cg",
                Some(tmp.path()),
                None,
            );
            oci.mounts
                .iter()
                .find(|m| m.destination == std::path::Path::new("/run/reliaburger/identity"))
                .expect("identity mount present")
                .source
                .clone()
                .expect("identity mount has a source")
        };

        let source_0 = identity_source("web-0");
        let source_1 = identity_source("web-1");
        assert_ne!(source_0, source_1, "replicas must not share a source");
        assert!(source_0.ends_with(".identity/web-0"));
        assert!(source_1.ends_with(".identity/web-1"));
    }

    #[test]
    fn generate_with_config_file_source() {
        let mut spec = minimal_app();
        spec.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: None,
            source: Some("/host/configs/app.conf".to_string()),
        });
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let cf_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/etc/app.conf"))
            .expect("config file mount not found");
        assert_eq!(
            cf_mount.source,
            Some(PathBuf::from("/host/configs/app.conf"))
        );
        assert!(cf_mount.options.contains(&"ro".to_string()));
    }

    #[test]
    fn generate_with_config_file_inline_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = minimal_app();
        spec.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: Some("key = value".to_string()),
            source: None,
        });
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            Some(tmp.path()),
            None,
        );

        let cf_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/etc/app.conf"))
            .expect("config file mount not found");
        let source = cf_mount
            .source
            .as_ref()
            .expect("inline config should have source");
        assert!(
            source.exists(),
            "inline config file should be written to disk"
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), "key = value");
        assert!(cf_mount.options.contains(&"ro".to_string()));
    }

    #[test]
    fn generate_with_volume_hostpath() {
        let mut spec = minimal_app();
        spec.volumes.push(VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("/host/data")),
            size: None,
        });
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let vol_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/data"))
            .expect("volume mount not found");
        assert_eq!(vol_mount.source, Some(PathBuf::from("/host/data")));
        assert!(vol_mount.options.contains(&"rw".to_string()));
    }

    #[test]
    fn generate_with_volume_managed() {
        let mut spec = minimal_app();
        spec.volumes.push(VolumeSpec {
            path: PathBuf::from("/data"),
            source: None,
            size: Some("10Gi".to_string()),
        });
        let volumes_dir = PathBuf::from("/var/lib/reliaburger/volumes");
        let oci = generate_oci_spec(
            "redis",
            "prod",
            &spec,
            "redis-0",
            None,
            "/cgroup/path",
            Some(&volumes_dir),
            None,
        );

        let vol_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/data"))
            .expect("volume mount not found");
        assert_eq!(
            vol_mount.source,
            Some(PathBuf::from(
                "/var/lib/reliaburger/volumes/prod/redis/data"
            ))
        );
        assert!(vol_mount.options.contains(&"rw".to_string()));
    }

    #[test]
    fn generate_with_multiple_volumes() {
        let mut spec = minimal_app();
        spec.volumes.push(VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("/host/data")),
            size: None,
        });
        spec.volumes.push(VolumeSpec {
            path: PathBuf::from("/logs"),
            source: None,
            size: None,
        });
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let data_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/data"));
        let logs_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/logs"));
        assert!(data_mount.is_some());
        assert!(logs_mount.is_some());
    }

    #[test]
    fn generate_serialises_to_json() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let json = serde_json::to_string_pretty(&oci).unwrap();
        assert!(json.contains("\"root\""));
        assert!(json.contains("\"process\""));
        assert!(json.contains("\"linux\""));
        assert!(json.contains("\"namespaces\""));
    }

    #[test]
    fn image_jobs_with_memory_limits_cannot_spill_into_swap() {
        for isolation in [
            crate::config::job::JobRuntime::Runc,
            crate::config::job::JobRuntime::SharedRunc,
        ] {
            let mut spec = minimal_job();
            spec.runtime = isolation;
            spec.memory = Some(crate::config::types::ResourceRange {
                request: 16 << 20,
                limit: 32 << 20,
            });
            let generated = generate_job_oci_spec("work", "default", &spec, "/cg", None);
            assert_eq!(
                generated
                    .linux
                    .resources
                    .unwrap()
                    .unified
                    .get("memory.swap.max")
                    .map(String::as_str),
                Some("0")
            );
        }
        let generated = generate_job_oci_spec("work", "default", &minimal_job(), "/cg", None);
        assert!(generated.linux.resources.is_none());
    }

    // -- generate_job_oci_spec ------------------------------------------------

    fn minimal_job() -> JobSpec {
        toml::from_str(
            r#"
            image = "myapp:v1"
            command = ["echo", "done"]
        "#,
        )
        .unwrap()
    }

    #[test]
    fn generate_job_minimal() {
        let spec = minimal_job();
        let oci = generate_job_oci_spec("migrate", "default", &spec, "/cgroup/path", None);

        assert_eq!(oci.root.path, "myapp:v1");
        assert_eq!(
            oci.process.args,
            vec!["echo".to_string(), "done".to_string()]
        );
        assert!(oci.process.env.is_empty());
        assert!(oci.linux.resources.is_none());
    }

    #[test]
    fn a_fresh_job_container_enforces_an_explicit_pids_limit() {
        let mut spec = minimal_job();
        spec.pids = Some(crate::config::types::PidsLimit::try_from(512).unwrap());
        let oci = generate_job_oci_spec("migrate", "default", &spec, "/cg", None);
        let resources = oci.linux.resources.clone().unwrap();
        assert_eq!(resources.pids, Some(OciPidsResources { limit: 512 }));
        // A pids limit alone sets no CPU or memory limit.
        assert_eq!((resources.cpu, resources.memory), (None, None));
        // runc reads the OCI field `linux.resources.pids.limit`.
        let json = serde_json::to_value(&oci).unwrap();
        assert_eq!(
            json["linux"]["resources"]["pids"],
            serde_json::json!({ "limit": 512 })
        );
        // It combines with CPU and memory rather than replacing them.
        spec.memory = Some(ResourceRange::parse_memory("64Mi").unwrap());
        let combined = generate_job_oci_spec("migrate", "default", &spec, "/cg", None)
            .linux
            .resources
            .unwrap();
        assert_eq!(combined.pids, Some(OciPidsResources { limit: 512 }));
        assert!(combined.memory.is_some());
    }

    // -- OOM score (#984) -----------------------------------------------------

    #[test]
    fn workloads_are_the_oom_killers_first_choice_before_bun() {
        // Bun's units ask for -900; every workload must sit well above it,
        // and above zero, so a global OOM takes a workload or a task first.
        const _: () = assert!(WORKLOAD_OOM_SCORE_ADJ > 0);
        const _: () = assert!(WORKLOAD_OOM_SCORE_ADJ <= 1000);
        let app = app_oci(&minimal_app());
        let host_app: AppSpec = toml::from_str(r#"exec = "/bin/true""#).unwrap();
        let host_app = app_oci(&host_app);
        let init = generate_init_oci_spec(&init_container(None, &["true"]), &app);
        let job = generate_job_oci_spec("migrate", "default", &minimal_job(), "/cg", None);
        let host_job: JobSpec = toml::from_str(r#"script = "true""#).unwrap();
        let host_job = generate_job_oci_spec("tidy", "default", &host_job, "/cg", None);
        for (what, spec) in [
            ("app", &app),
            ("host app", &host_app),
            ("init container", &init),
            ("job", &job),
            ("host job", &host_job),
        ] {
            assert_eq!(
                spec.process.oom_score_adj,
                Some(WORKLOAD_OOM_SCORE_ADJ),
                "{what}"
            );
        }
    }

    #[test]
    fn oom_score_is_written_as_the_oci_process_field() {
        // runc reads `process.oomScoreAdj` from config.json.
        let job = generate_job_oci_spec("migrate", "default", &minimal_job(), "/cg", None);
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(
            json["process"]["oomScoreAdj"],
            serde_json::json!(WORKLOAD_OOM_SCORE_ADJ)
        );
        // A spec without one (an internal runtime command) leaves the field
        // out, so the process keeps the score it inherits.
        let mut inherited = job.clone();
        inherited.process.oom_score_adj = None;
        let json = serde_json::to_value(&inherited).unwrap();
        assert!(json["process"].get("oomScoreAdj").is_none(), "{json}");
        let back: OciSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.process.oom_score_adj, None);
    }

    #[test]
    fn generate_job_has_standard_mounts() {
        let spec = minimal_job();
        let oci = generate_job_oci_spec("migrate", "default", &spec, "/cgroup/path", None);

        let mount_paths: Vec<&str> = oci
            .mounts
            .iter()
            .map(|m| m.destination.to_str().unwrap())
            .collect();
        assert!(mount_paths.contains(&"/proc"));
        assert!(mount_paths.contains(&"/dev"));
        assert!(mount_paths.contains(&"/sys"));
    }

    #[test]
    fn generate_job_with_no_command() {
        let spec: JobSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        let oci = generate_job_oci_spec("cleanup", "default", &spec, "/cgroup/path", None);

        assert!(oci.process.args.is_empty());
    }

    // -- OciIdMapping ----------------------------------------------------------

    #[test]
    fn oci_id_mapping_serialises_correctly() {
        let mapping = OciIdMapping {
            container_id: 0,
            host_id: 1000,
            size: 1,
        };
        let json = serde_json::to_value(&mapping).unwrap();
        assert_eq!(json["containerID"], 0);
        assert_eq!(json["hostID"], 1000);
        assert_eq!(json["size"], 1);
    }

    #[test]
    fn uid_gid_mappings_omitted_when_none() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );

        let json = serde_json::to_string(&oci).unwrap();
        assert!(!json.contains("uidMappings"));
        assert!(!json.contains("gidMappings"));
    }

    // -- Network namespace path -----------------------------------------------

    #[test]
    fn standard_namespaces_without_netns_path() {
        let ns = standard_namespaces(None);
        let net = ns.iter().find(|n| n.ns_type == "network").unwrap();
        assert!(net.path.is_none());
    }

    #[test]
    fn standard_namespaces_with_netns_path() {
        let ns = standard_namespaces(Some("/var/run/netns/rb-web-0"));
        let net = ns.iter().find(|n| n.ns_type == "network").unwrap();
        assert_eq!(net.path.as_deref(), Some("/var/run/netns/rb-web-0"));
    }

    #[test]
    fn generate_with_netns_path_sets_network_namespace() {
        let spec = minimal_app();
        let oci = generate_oci_spec(
            "web",
            "default",
            &spec,
            "web-0",
            None,
            "/cgroup/path",
            None,
            Some("/var/run/netns/rb-web-0"),
        );

        let net_ns = oci
            .linux
            .namespaces
            .iter()
            .find(|n| n.ns_type == "network")
            .unwrap();
        assert_eq!(net_ns.path.as_deref(), Some("/var/run/netns/rb-web-0"));
    }

    // -- Secret decryption ---------------------------------------------------

    #[test]
    fn build_env_with_decryptor_decrypts_secrets() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            PLAIN = "hello"
            SECRET = "ENC[AGE:encrypted-data]"
            "#,
        )
        .unwrap();

        let decryptor: SecretDecryptor = Box::new(|_encrypted| Ok("decrypted-value".to_string()));
        let env = build_env_with_decryptor(&spec, Some(&decryptor)).unwrap();

        assert!(env.contains(&"PLAIN=hello".to_string()));
        assert!(env.contains(&"SECRET=decrypted-value".to_string()));
    }

    #[test]
    fn build_env_with_decryptor_fails_closed_on_error() {
        // M4: a decryption failure is an error, so the caller blocks the start
        // — it no longer injects `SECRET=DECRYPT_ERROR:...` and runs anyway.
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            SECRET = "ENC[AGE:bad-data]"
            "#,
        )
        .unwrap();

        let decryptor: SecretDecryptor = Box::new(|_encrypted| Err("key not found".to_string()));
        let result = build_env_with_decryptor(&spec, Some(&decryptor));
        let err = result.unwrap_err();
        assert!(err.contains("SECRET"), "{err}");
        assert!(err.contains("key not found"), "{err}");
    }

    #[test]
    fn build_env_without_decryptor_passes_through() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            SECRET = "ENC[AGE:abc123]"
            "#,
        )
        .unwrap();

        let env = build_env_with_decryptor(&spec, None).unwrap();
        assert!(env.contains(&"SECRET=ENC[AGE:abc123]".to_string()));
    }

    // -- Journal copies of a retired spec -------------------------------------

    fn spec_with_env(env: &[&str]) -> OciSpec {
        let mut oci = generate_oci_spec(
            "web",
            "default",
            &minimal_app(),
            "web-0",
            None,
            "/cgroup/path",
            None,
            None,
        );
        oci.process.env = env.iter().map(|entry| entry.to_string()).collect();
        oci
    }

    #[test]
    fn without_environment_values_keeps_only_variable_names() {
        let oci = spec_with_env(&["SECRET=plaintext", "EMPTY=", "URL=a=b"]);
        let scrubbed = oci.without_environment_values();
        assert_eq!(scrubbed.process.env, vec!["SECRET", "EMPTY", "URL"]);
        assert_eq!(scrubbed.process.args, oci.process.args);
        assert_eq!(scrubbed.linux, oci.linux);
        assert_eq!(scrubbed.without_environment_values(), scrubbed);
    }

    #[test]
    fn a_spec_matches_its_live_and_its_scrubbed_journal_copy() {
        let oci = spec_with_env(&["SECRET=plaintext", "PLAIN=visible"]);
        assert!(oci.matches_journal(&oci));
        assert!(oci.matches_journal(&oci.without_environment_values()));
    }

    #[test]
    fn a_live_journal_copy_must_match_every_environment_value() {
        let journal = spec_with_env(&["SECRET=plaintext"]);
        assert!(!spec_with_env(&["SECRET=other"]).matches_journal(&journal));
        assert!(!spec_with_env(&["SECRET"]).matches_journal(&journal));
    }

    #[test]
    fn a_scrubbed_journal_copy_still_refuses_a_different_request() {
        let journal = spec_with_env(&["SECRET=plaintext"]).without_environment_values();
        assert!(!spec_with_env(&["OTHER=plaintext"]).matches_journal(&journal));
        assert!(!spec_with_env(&[]).matches_journal(&journal));
        let mut moved = spec_with_env(&["SECRET=plaintext"]);
        moved.process.args = vec!["elsewhere".into()];
        assert!(!moved.matches_journal(&journal));
    }

    #[test]
    fn all_workload_generators_preserve_the_host_cgroup_identity() {
        let path = "/sys/fs/cgroup/reliaburger/test/web/3";
        let specifications = [
            generate_oci_spec(
                "web",
                "test",
                &minimal_app(),
                "test__web-3",
                None,
                path,
                None,
                None,
            ),
            generate_job_oci_spec("web", "test", &minimal_job(), path, None),
            generate_init_oci_spec(
                &init_container(None, &["true"]),
                &generate_oci_spec(
                    "web",
                    "test",
                    &minimal_app(),
                    "test__web-3",
                    None,
                    path,
                    None,
                    None,
                ),
            ),
        ];
        for specification in specifications {
            assert_eq!(
                specification.linux.cgroups_path.as_deref(),
                Some("/reliaburger/test/web/3")
            );
            assert_eq!(
                specification.linux.host_cgroup_path().as_deref(),
                Some(Path::new(path))
            );
        }
    }

    #[test]
    fn recovered_cgroup_paths_refuse_ambiguous_or_legacy_identity() {
        let mut specification =
            generate_init_oci_spec(&init_container(None, &[]), &app_oci(&minimal_app()));
        for invalid in [
            "",
            "/",
            "relative",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/",
            "/sys/fs/cgroup/web",
        ] {
            specification.linux.cgroups_path = Some(invalid.into());
            assert!(
                specification.linux.host_cgroup_path().is_none(),
                "accepted {invalid:?}"
            );
        }
        specification.linux.cgroups_path = None;
        assert!(specification.linux.host_cgroup_path().is_none());
    }
}

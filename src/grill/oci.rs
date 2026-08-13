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

/// A simplified OCI runtime specification.
///
/// Contains the fields Reliaburger actually uses. Additional fields
/// are added as later phases require them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciSpec {
    pub root: OciRoot,
    pub process: OciProcess,
    pub mounts: Vec<OciMount>,
    pub linux: OciLinux,
    /// Host-port publication for the workload, when the app declares a
    /// port. Not part of the OCI runtime spec proper — runtimes with
    /// per-container networking (runc) read it to install the DNAT map
    /// element alongside the network namespace. `#[serde(default)]`
    /// keeps instance records written before this field readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_mapping: Option<PortMapping>,
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
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: OciUser,
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
    #[serde(rename = "cgroupsPath", skip_serializing_if = "Option::is_none")]
    pub cgroups_path: Option<String>,
    #[serde(rename = "uidMappings", skip_serializing_if = "Option::is_none")]
    pub uid_mappings: Option<Vec<OciIdMapping>>,
    #[serde(rename = "gidMappings", skip_serializing_if = "Option::is_none")]
    pub gid_mappings: Option<Vec<OciIdMapping>>,
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
) -> OciSpec {
    let env = build_env_with_decryptor(spec, decryptor);
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

    OciSpec {
        root: OciRoot {
            // Process workloads (exec/script) use the host root filesystem.
            // Container workloads use the image reference (Apple Container)
            // or resolved rootfs path (runc).
            path: if spec.exec.is_some() || spec.script.is_some() {
                "proc-grill:host".to_string()
            } else {
                spec.image.clone().unwrap_or_else(|| {
                    format!("/var/lib/reliaburger/images/{namespace}/{app_name}/rootfs")
                })
            },
            readonly: false,
        },
        process: OciProcess {
            args,
            env,
            cwd: "/".to_string(),
            // Using nobody (65534) as the container user. A custom `burger`
            // user would require creating it inside each container rootfs
            // before exec, which adds complexity for no security benefit —
            // 65534 is already unprivileged and widely recognised.
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
        },
        mounts,
        linux: OciLinux {
            namespaces,
            resources,
            cgroups_path: Some(cgroup_path.to_string()),
            uid_mappings: None,
            gid_mappings: None,
        },
        port_mapping: host_port.zip(spec.port).map(|(hp, cp)| PortMapping {
            host_port: hp,
            container_port: cp,
        }),
    }
}

/// A callback for decrypting `ENC[AGE:...]` values.
///
/// Accepts the encrypted string (including the `ENC[AGE:...]` wrapper)
/// and returns the decrypted plaintext. If decryption fails, the error
/// message is used instead (prefixed with `DECRYPT_ERROR:`).
pub type SecretDecryptor = Box<dyn Fn(&str) -> Result<String, String>>;

/// Build env vars with optional secret decryption.
///
/// When a `SecretDecryptor` is provided, `ENC[AGE:...]` values are
/// decrypted before injection. Without a decryptor, they're passed
/// through as literal strings (pre-Phase 4 behaviour).
pub fn build_env_with_decryptor(
    spec: &AppSpec,
    decryptor: Option<&SecretDecryptor>,
) -> Vec<String> {
    let mut env = Vec::new();
    for (key, value) in &spec.env {
        match value {
            EnvValue::Plain(v) => env.push(format!("{key}={v}")),
            EnvValue::Encrypted(v) => {
                if let Some(decrypt) = decryptor {
                    match decrypt(v) {
                        Ok(plaintext) => env.push(format!("{key}={plaintext}")),
                        Err(e) => env.push(format!("{key}=DECRYPT_ERROR:{e}")),
                    }
                } else {
                    // No decryptor available — pass through as-is
                    env.push(format!("{key}={v}"));
                }
            }
        }
    }
    env
}

/// Build the process arguments from an app spec.
///
/// Returns the app's `command` field if set. When empty, ProcessGrill
/// falls back to `sleep 86400`; real runtimes (runc, Apple Container)
/// use the image's entrypoint instead.
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

    spec.command.clone()
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
    for cf in &spec.config_file {
        let source = if let Some(ref source_path) = cf.source {
            Some(PathBuf::from(source_path))
        } else if let Some(ref content) = cf.content {
            // Resolve inline content to a temp file path
            let base = volumes_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/var/lib/reliaburger/volumes"));
            let config_dir = base.join(".config").join(namespace).join(app_name);
            let filename = cf
                .path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("config"));
            let host_path = config_dir.join(filename);
            if let Err(e) = std::fs::create_dir_all(&config_dir) {
                eprintln!("failed to create config dir {}: {e}", config_dir.display());
            } else if let Err(e) = std::fs::write(&host_path, content.as_bytes()) {
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

    let resources = build_resources_from_ranges(spec.cpu.as_ref(), spec.memory.as_ref());

    OciSpec {
        root: OciRoot {
            path: if spec.exec.is_some() || spec.script.is_some() {
                "proc-grill:host".to_string()
            } else {
                spec.image.clone().unwrap_or_else(|| {
                    format!("/var/lib/reliaburger/images/{namespace}/{job_name}/rootfs")
                })
            },
            readonly: false,
        },
        process: OciProcess {
            args,
            env,
            cwd: "/".to_string(),
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
        },
        mounts: standard_mounts(),
        linux: OciLinux {
            namespaces: standard_namespaces(netns_path),
            resources,
            cgroups_path: Some(cgroup_path.to_string()),
            uid_mappings: None,
            gid_mappings: None,
        },
        // Jobs run to completion and don't publish ports.
        port_mapping: None,
    }
}

/// Generate a minimal OCI spec for an init container.
///
/// Init containers run a single command to completion before the main
/// app starts. No ports, no health checks, no volumes. The `image`
/// parameter is typically inherited from the parent app's image.
pub fn generate_init_oci_spec(
    command: &[String],
    namespace: &str,
    app_name: &str,
    image: Option<&str>,
    cgroup_path: &str,
    netns_path: Option<&str>,
) -> OciSpec {
    OciSpec {
        root: OciRoot {
            path: image.map(String::from).unwrap_or_else(|| {
                format!("/var/lib/reliaburger/images/{namespace}/{app_name}/rootfs")
            }),
            readonly: false,
        },
        process: OciProcess {
            args: command.to_vec(),
            env: Vec::new(),
            cwd: "/".to_string(),
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
        },
        mounts: standard_mounts(),
        linux: OciLinux {
            namespaces: standard_namespaces(netns_path),
            resources: None,
            cgroups_path: Some(cgroup_path.to_string()),
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
    fn port_mapping_survives_record_round_trip_and_old_records_default() {
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

        // …and records written before the field existed still parse.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("port_mapping");
        let old: OciSpec = serde_json::from_value(value).unwrap();
        assert_eq!(old.port_mapping, None);
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
        );

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
            Some("/sys/fs/cgroup/reliaburger/default/web/0".to_string())
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
        let env = build_env_with_decryptor(&spec, Some(&decryptor));

        assert!(env.contains(&"PLAIN=hello".to_string()));
        assert!(env.contains(&"SECRET=decrypted-value".to_string()));
    }

    #[test]
    fn build_env_with_decryptor_handles_error() {
        let spec: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            [env]
            SECRET = "ENC[AGE:bad-data]"
            "#,
        )
        .unwrap();

        let decryptor: SecretDecryptor = Box::new(|_encrypted| Err("key not found".to_string()));
        let env = build_env_with_decryptor(&spec, Some(&decryptor));

        assert!(env[0].starts_with("SECRET=DECRYPT_ERROR:"));
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

        let env = build_env_with_decryptor(&spec, None);
        assert!(env.contains(&"SECRET=ENC[AGE:abc123]".to_string()));
    }
}

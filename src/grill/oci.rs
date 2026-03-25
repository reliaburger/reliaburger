/// OCI runtime specification generation.
///
/// Generates a simplified OCI runtime spec from a `config::AppSpec`.
/// We define our own types rather than importing the full OCI spec
/// crate, because we only need a subset and want control over the
/// serialisation and derives.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
use crate::config::job::JobSpec;
use crate::config::types::EnvValue;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OciResources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<OciCpuResources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<OciMemoryResources>,
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
/// as the literal encrypted string. Decryption requires Sesame PKI
/// (Phase 4).
pub fn generate_oci_spec(
    app_name: &str,
    namespace: &str,
    spec: &AppSpec,
    host_port: Option<u16>,
    cgroup_path: &str,
    volumes_dir: Option<&Path>,
    netns_path: Option<&str>,
) -> OciSpec {
    let env = build_env(spec);
    let args = build_args(app_name, spec);
    let mounts = build_mounts(spec, host_port, app_name, namespace, volumes_dir);

    let namespaces = standard_namespaces(netns_path);

    let resources = build_resources(spec);

    OciSpec {
        root: OciRoot {
            // Use the image reference directly (Apple Container needs this).
            // For runc, Phase 5 (Pickle) will resolve the image to a local rootfs.
            path: spec.image.clone().unwrap_or_else(|| {
                format!("/var/lib/reliaburger/images/{namespace}/{app_name}/rootfs")
            }),
            readonly: false,
        },
        process: OciProcess {
            args,
            env,
            cwd: "/".to_string(),
            // TODO(Phase 4): use the `burger` unprivileged user
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
    }
}

fn build_env(spec: &AppSpec) -> Vec<String> {
    let mut env = Vec::new();
    for (key, value) in &spec.env {
        match value {
            EnvValue::Plain(v) => env.push(format!("{key}={v}")),
            // TODO(Phase 4): decrypt via Sesame PKI. For now, pass
            // the encrypted blob through. The container will see the
            // literal ENC[AGE:...] string until Sesame is implemented.
            EnvValue::Encrypted(v) => env.push(format!("{key}={v}")),
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
    spec.command.clone()
}

fn build_mounts(
    spec: &AppSpec,
    _host_port: Option<u16>,
    app_name: &str,
    namespace: &str,
    volumes_dir: Option<&Path>,
) -> Vec<OciMount> {
    let mut mounts = standard_mounts();

    // Config files: read-only bind mounts
    for cf in &spec.config_file {
        mounts.push(OciMount {
            destination: cf.path.clone(),
            // TODO(Phase 5): resolve inline content to a temp file path
            source: cf.source.as_ref().map(PathBuf::from),
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
    let cpu = spec.cpu.as_ref().map(|range| OciCpuResources {
        quota: (range.limit * 100_000 / 1000) as i64,
        period: 100_000,
    });

    let memory = spec.memory.as_ref().map(|range| OciMemoryResources {
        limit: range.limit as i64,
    });

    if cpu.is_some() || memory.is_some() {
        Some(OciResources { cpu, memory })
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

    let args = spec.command.clone().unwrap_or_default();

    let cpu = spec.cpu.as_ref().map(|range| OciCpuResources {
        quota: (range.limit * 100_000 / 1000) as i64,
        period: 100_000,
    });
    let memory = spec.memory.as_ref().map(|range| OciMemoryResources {
        limit: range.limit as i64,
    });
    let resources = if cpu.is_some() || memory.is_some() {
        Some(OciResources { cpu, memory })
    } else {
        None
    };

    OciSpec {
        root: OciRoot {
            path: spec.image.clone().unwrap_or_else(|| {
                format!("/var/lib/reliaburger/images/{namespace}/{job_name}/rootfs")
            }),
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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

        assert_eq!(oci.root.path, "test:v1");
        assert_eq!(oci.process.cwd, "/");
        assert_eq!(oci.process.user.uid, 65534);
        assert!(oci.process.env.is_empty());
    }

    #[test]
    fn generate_without_image_uses_filesystem_path() {
        let spec: AppSpec = toml::from_str(r#"command = ["echo", "hi"]"#).unwrap();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

        assert!(
            oci.process
                .env
                .contains(&"SECRET=ENC[AGE:abc123]".to_string())
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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

        let resources = oci.linux.resources.unwrap();
        let memory = resources.memory.unwrap();
        assert_eq!(memory.limit, 512 * 1024 * 1024);
    }

    #[test]
    fn generate_without_resources_has_no_resources_block() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);
        assert!(oci.linux.resources.is_none());
    }

    #[test]
    fn generate_has_all_namespaces() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
    fn generate_with_config_file() {
        let mut spec = minimal_app();
        spec.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: Some("key = value".to_string()),
            source: None,
        });
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

        let cf_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == std::path::Path::new("/etc/app.conf"));
        assert!(cf_mount.is_some());
        assert!(cf_mount.unwrap().options.contains(&"ro".to_string()));
    }

    #[test]
    fn generate_with_volume_hostpath() {
        let mut spec = minimal_app();
        spec.volumes.push(VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("/host/data")),
            size: None,
        });
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path", None, None);

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
}

/// OCI runtime specification generation.
///
/// Generates a simplified OCI runtime spec from a `config::AppSpec`.
/// We define our own types rather than importing the full OCI spec
/// crate, because we only need a subset and want control over the
/// serialisation and derives.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;
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
) -> OciSpec {
    let env = build_env(spec);
    let args = build_args(app_name, spec);
    let mounts = build_mounts(spec, host_port);

    let namespaces = vec![
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
            path: None, // TODO(Phase 3): set to the container's network namespace path
        },
    ];

    let resources = build_resources(spec);

    OciSpec {
        root: OciRoot {
            // TODO(Phase 5): resolve via Pickle image cache
            path: format!("/var/lib/reliaburger/images/{namespace}/{app_name}/rootfs"),
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

fn build_args(app_name: &str, spec: &AppSpec) -> Vec<String> {
    // If the image defines an entrypoint, containerd uses it.
    // We only set args if the app has an explicit command
    // (which isn't modelled in config::AppSpec yet for apps,
    // only for jobs). For now, use a placeholder.
    let _ = (app_name, spec);
    Vec::new()
}

fn build_mounts(spec: &AppSpec, _host_port: Option<u16>) -> Vec<OciMount> {
    let mut mounts = vec![
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
    ];

    // Config files: read-only bind mounts
    for cf in &spec.config_file {
        mounts.push(OciMount {
            destination: cf.path.clone(),
            // TODO(Phase 1): resolve source content to a temp file path
            source: cf.source.as_ref().map(PathBuf::from),
            mount_type: Some("bind".to_string()),
            options: vec!["bind".to_string(), "ro".to_string()],
        });
    }

    // Volume: read-write bind mount
    if let Some(vol) = &spec.volume {
        mounts.push(OciMount {
            destination: vol.path.clone(),
            // TODO(Phase 1): resolve to actual host path under storage.volumes
            source: Some(vol.path.clone()),
            mount_type: Some("bind".to_string()),
            options: vec!["bind".to_string(), "rw".to_string()],
        });
    }

    mounts
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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

        assert_eq!(
            oci.root.path,
            "/var/lib/reliaburger/images/default/web/rootfs"
        );
        assert_eq!(oci.process.cwd, "/");
        assert_eq!(oci.process.user.uid, 65534);
        assert!(oci.process.env.is_empty());
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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

        let resources = oci.linux.resources.unwrap();
        let memory = resources.memory.unwrap();
        assert_eq!(memory.limit, 512 * 1024 * 1024);
    }

    #[test]
    fn generate_without_resources_has_no_resources_block() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");
        assert!(oci.linux.resources.is_none());
    }

    #[test]
    fn generate_has_all_namespaces() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

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
        );

        assert_eq!(
            oci.linux.cgroups_path,
            Some("/sys/fs/cgroup/reliaburger/default/web/0".to_string())
        );
    }

    #[test]
    fn generate_has_standard_mounts() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

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
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

        let cf_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == PathBuf::from("/etc/app.conf"));
        assert!(cf_mount.is_some());
        assert!(cf_mount.unwrap().options.contains(&"ro".to_string()));
    }

    #[test]
    fn generate_with_volume() {
        let mut spec = minimal_app();
        spec.volume = Some(VolumeSpec {
            path: PathBuf::from("/data"),
            size: "10Gi".to_string(),
        });
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

        let vol_mount = oci
            .mounts
            .iter()
            .find(|m| m.destination == PathBuf::from("/data"));
        assert!(vol_mount.is_some());
        assert!(vol_mount.unwrap().options.contains(&"rw".to_string()));
    }

    #[test]
    fn generate_serialises_to_json() {
        let spec = minimal_app();
        let oci = generate_oci_spec("web", "default", &spec, None, "/cgroup/path");

        let json = serde_json::to_string_pretty(&oci).unwrap();
        assert!(json.contains("\"root\""));
        assert!(json.contains("\"process\""));
        assert!(json.contains("\"linux\""));
        assert!(json.contains("\"namespaces\""));
    }
}

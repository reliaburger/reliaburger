//! Node policy for host-path mounts (`source` on a volume or config file).
//!
//! A host-path `source` bind-mounts a directory or file of the node into a
//! container exactly as given. Every rootful container on a node shares one
//! user-namespace id range, and managed volumes are chowned into it, so an
//! unrestricted `source` lets any deployer read or rewrite another tenant's
//! managed volume. The node therefore refuses every host path unless an
//! operator lists a prefix that covers it in `[storage] allowed_host_paths`,
//! and it always refuses the node's own storage and security directories,
//! even when a listed prefix contains them.

use std::path::{Component, Path, PathBuf};

/// Why the node refused a host-path `source`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostPathRefusal {
    /// A relative source has no fixed meaning on the node.
    #[error("host path {path:?} must be absolute")]
    NotAbsolute { path: PathBuf },
    /// `..` could walk out of a listed prefix, so it is never accepted.
    #[error("host path {path:?} must not contain '..' components")]
    ParentComponent { path: PathBuf },
    /// No `[storage] allowed_host_paths` prefix covers the source.
    #[error(
        "host path {path:?} isn't under any of this node's [storage] allowed_host_paths \
         (an empty list refuses every host path)"
    )]
    NotAllowed { path: PathBuf },
    /// The source is, contains or sits under one of the node's own directories.
    #[error(
        "host path {path:?} overlaps this node's own directory {directory:?}, \
         which no workload may mount"
    )]
    Protected { path: PathBuf, directory: PathBuf },
}

/// Which host paths this node lets a workload bind-mount.
///
/// The default refuses every host path: there are no allowed prefixes.
#[derive(Debug, Clone, Default)]
pub struct HostPathPolicy {
    allowed: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

impl HostPathPolicy {
    /// A policy admitting sources under `allowed`, never overlapping `protected`.
    pub fn new(allowed: Vec<PathBuf>, protected: Vec<PathBuf>) -> Self {
        Self {
            allowed: allowed.iter().map(|path| resolve(path)).collect(),
            protected: protected.iter().map(|path| resolve(path)).collect(),
        }
    }

    /// The policy `node.toml` describes: `[storage] allowed_host_paths`, with
    /// the storage directories, the identity directory, the directories holding
    /// the master key and security bootstrap, and the script directory protected.
    pub fn from_node_config(config: &crate::config::NodeConfig) -> Self {
        let storage = &config.storage;
        let security = &config.security;
        let mut protected = vec![
            storage.data.clone(),
            storage.images.clone(),
            storage.logs.clone(),
            storage.metrics.clone(),
            storage.volumes.clone(),
            security
                .identity_dir
                .clone()
                .unwrap_or_else(|| crate::sesame::identity_store::identity_dir(&storage.data)),
            config.process_workloads.script_dir.clone(),
        ];
        // The key files live in an operator-chosen directory. Protect the
        // file and that directory, unless the directory is the filesystem
        // root: protecting `/` would refuse every host path.
        for file in [&security.master_key_path, &security.bootstrap_path]
            .into_iter()
            .flatten()
        {
            protected.push(file.clone());
            if let Some(parent) = file.parent()
                && parent.parent().is_some()
            {
                protected.push(parent.to_path_buf());
            }
        }
        Self::new(storage.allowed_host_paths.clone(), protected)
    }

    /// Admit or refuse one host-path `source`.
    ///
    /// Prefixes match by whole path components after symlinks are resolved,
    /// so `/srv/import-evil` is not under `/srv/import`. A source must not be,
    /// sit under, or contain a protected directory.
    pub fn admit(&self, source: &Path) -> Result<(), HostPathRefusal> {
        if !source.is_absolute() {
            return Err(HostPathRefusal::NotAbsolute {
                path: source.to_path_buf(),
            });
        }
        if source.components().any(|c| c == Component::ParentDir) {
            return Err(HostPathRefusal::ParentComponent {
                path: source.to_path_buf(),
            });
        }
        let resolved = resolve(source);
        if let Some(directory) = self
            .protected
            .iter()
            .find(|directory| resolved.starts_with(directory) || directory.starts_with(&resolved))
        {
            return Err(HostPathRefusal::Protected {
                path: source.to_path_buf(),
                directory: directory.clone(),
            });
        }
        if self
            .allowed
            .iter()
            .any(|prefix| resolved.starts_with(prefix))
        {
            Ok(())
        } else {
            Err(HostPathRefusal::NotAllowed {
                path: source.to_path_buf(),
            })
        }
    }
}

/// Resolve symlinks in the longest prefix of `path` that exists, keeping the
/// rest as written. A source may not exist yet, but a symlinked directory
/// above it must still count as the place it points at.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |resolved, part| resolved.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&Path], volumes: &Path) -> HostPathPolicy {
        HostPathPolicy::new(
            allowed.iter().map(|path| path.to_path_buf()).collect(),
            vec![volumes.to_path_buf()],
        )
    }

    #[test]
    fn host_path_refused_when_allowlist_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let policy = HostPathPolicy::default();
        assert!(matches!(
            policy.admit(&root.path().join("data")),
            Err(HostPathRefusal::NotAllowed { .. })
        ));
    }

    #[test]
    fn host_path_admitted_under_a_listed_prefix() {
        let root = tempfile::tempdir().unwrap();
        let import = root.path().join("srv/import");
        std::fs::create_dir_all(import.join("daily")).unwrap();
        let policy = policy(&[&import], &root.path().join("volumes"));
        policy.admit(&import).unwrap();
        policy.admit(&import.join("daily")).unwrap();
        // A source that doesn't exist yet is judged by where it would be.
        policy.admit(&import.join("weekly/report.csv")).unwrap();
    }

    #[test]
    fn prefix_match_is_by_path_component_not_string() {
        let root = tempfile::tempdir().unwrap();
        let policy = policy(&[&root.path().join("srv/import")], &root.path().join("v"));
        assert!(matches!(
            policy.admit(&root.path().join("srv/import-evil")),
            Err(HostPathRefusal::NotAllowed { .. })
        ));
    }

    #[test]
    fn host_path_under_the_volumes_directory_is_refused_even_when_listed() {
        let root = tempfile::tempdir().unwrap();
        let volumes = root.path().join("volumes");
        std::fs::create_dir_all(volumes.join("team-b/db/data")).unwrap();
        // The operator lists a parent of the volumes directory.
        let policy = policy(&[root.path()], &volumes);
        for source in [
            volumes.join("team-b/db"),
            volumes.clone(),
            // Mounting a parent would expose every managed volume too.
            root.path().to_path_buf(),
        ] {
            assert!(
                matches!(
                    policy.admit(&source),
                    Err(HostPathRefusal::Protected { .. })
                ),
                "{source:?}"
            );
        }
        policy.admit(&root.path().join("elsewhere")).unwrap();
    }

    #[test]
    fn dot_dot_in_source_cannot_escape_a_listed_prefix() {
        let root = tempfile::tempdir().unwrap();
        let import = root.path().join("srv/import");
        std::fs::create_dir_all(&import).unwrap();
        let policy = policy(&[&import], &root.path().join("volumes"));
        assert!(matches!(
            policy.admit(&import.join("../../volumes/team-b")),
            Err(HostPathRefusal::ParentComponent { .. })
        ));
    }

    #[test]
    fn a_relative_source_is_refused() {
        let policy = HostPathPolicy::new(vec![PathBuf::from("/")], Vec::new());
        assert!(matches!(
            policy.admit(Path::new("configs/app.yaml")),
            Err(HostPathRefusal::NotAbsolute { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_listed_prefix_cannot_reach_the_volumes_directory() {
        let root = tempfile::tempdir().unwrap();
        let import = root.path().join("srv/import");
        let volumes = root.path().join("volumes");
        std::fs::create_dir_all(&import).unwrap();
        std::fs::create_dir_all(volumes.join("team-b")).unwrap();
        std::os::unix::fs::symlink(&volumes, import.join("link")).unwrap();
        let policy = policy(&[&import], &volumes);
        assert!(matches!(
            policy.admit(&import.join("link/team-b")),
            Err(HostPathRefusal::Protected { .. })
        ));
    }

    #[test]
    fn node_config_protects_its_storage_and_security_directories() {
        let mut config = crate::config::NodeConfig::default();
        config.storage.allowed_host_paths = vec![PathBuf::from("/var/lib"), PathBuf::from("/etc")];
        config.security.master_key_path = Some(PathBuf::from("/etc/reliaburger/c-master.key"));
        let policy = HostPathPolicy::from_node_config(&config);
        for source in [
            "/var/lib/reliaburger/volumes/team-b/db",
            "/var/lib/reliaburger/data/identity",
            "/var/lib/reliaburger",
            "/etc/reliaburger/c-master.key",
        ] {
            assert!(
                matches!(
                    policy.admit(Path::new(source)),
                    Err(HostPathRefusal::Protected { .. })
                ),
                "{source}"
            );
        }
        policy.admit(Path::new("/var/lib/shared-imports")).unwrap();
    }
}

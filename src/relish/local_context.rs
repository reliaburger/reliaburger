//! Private host credentials for the managed laptop cluster.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::RelishError;

const MAX_CONTEXT_BYTES: u64 = 64 * 1024;

/// Credentials and endpoint belonging to one managed cluster operation.
/// This type deliberately omits `Debug` so diagnostics can't print the bearer.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalContext {
    /// Persistence format, currently 1.
    pub schema: u32,
    /// Stable cluster operation identifier, protecting another cluster's context.
    pub owner: String,
    /// Host-accessible HTTPS API endpoint.
    pub endpoint: String,
    /// Administrator bearer, stored only in an owner-readable file.
    pub token: String,
    /// Absolute path to the cluster's public CA certificate.
    pub ca_cert: PathBuf,
    /// Explicit host forwards; an absent origin never implies guest reachability.
    pub service_endpoints: crate::bun::capabilities::ServiceEndpoints,
}

impl LocalContext {
    /// Load an optional context, refusing malformed or exposed credentials.
    pub fn load(path: &Path) -> Result<Option<Self>, RelishError> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                return Err(RelishError::InitFailed(format!(
                    "context {} contains credentials; restrict its permissions to 0600",
                    path.display()
                )));
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONTEXT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONTEXT_BYTES {
            return Err(RelishError::InitFailed(
                "local context exceeds 64 KiB".to_string(),
            ));
        }
        let context: Self = serde_json::from_slice(&bytes)
            .map_err(|error| RelishError::InitFailed(format!("invalid local context: {error}")))?;
        context.validate()?;
        Ok(Some(context))
    }

    /// Atomically save credentials, without replacing another cluster's context.
    pub fn save(&self, path: &Path) -> Result<(), RelishError> {
        self.validate()?;
        let parent = path.parent().ok_or_else(|| {
            RelishError::InitFailed("context has no parent directory".to_string())
        })?;
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(parent)?;
        let _lock = lock_context(path)?;
        if let Some(existing) = Self::load(path)?
            && existing.owner != self.owner
        {
            return Err(RelishError::InitFailed(
                "another managed cluster owns the active context".to_string(),
            ));
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(RelishError::SerialiseJson)?;
        crate::sesame::identity::atomic_write_mode(path, &bytes, Some(0o600))?;
        Ok(())
    }

    /// Remove the active credentials only when they belong to this operation.
    pub fn remove_owned(path: &Path, owner: &str) -> Result<(), RelishError> {
        let _lock = lock_context(path)?;
        if Self::load(path)?.is_some_and(|context| context.owner == owner) {
            std::fs::remove_file(path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }

    /// Connect using this cluster's pinned CA and bearer, with explicit operator
    /// overrides taking precedence over saved values.
    pub fn client(
        &self,
        token: Option<&str>,
        ca_cert: Option<&Path>,
    ) -> Result<super::client::BunClient, RelishError> {
        self.validate()?;
        let ca = std::fs::read(ca_cert.unwrap_or(&self.ca_cert))?;
        super::client::BunClient::new_with_ca(
            &self.endpoint,
            Some(token.unwrap_or(&self.token)),
            &ca,
        )
        .map(|client| client.with_service_endpoints(self.service_endpoints.clone()))
    }

    /// This context's host forwards, for a connection pinned to the same
    /// cluster CA.
    ///
    /// `--endpoint` bypasses the context's credentials so an operator can
    /// talk to any node, say node 2 through its own API forward. The
    /// registry and ingress forwards are still how this host reaches that
    /// cluster, and without them relish falls back to the node's own
    /// listener address, which means nothing outside the VM. The CA is the
    /// identity check: a connection that trusts the same root CA talks to
    /// the same cluster. Anything else (no CA, another CA, an unreadable
    /// file) gets no forwards.
    pub fn forwards_for_ca(
        &self,
        connection_ca_pem: Option<&[u8]>,
    ) -> Option<crate::bun::capabilities::ServiceEndpoints> {
        let connection_ca_pem = connection_ca_pem?;
        let context_ca_pem = std::fs::read(&self.ca_cert).ok()?;
        (context_ca_pem == connection_ca_pem).then(|| self.service_endpoints.clone())
    }

    fn validate(&self) -> Result<(), RelishError> {
        if self.schema != 1
            || self.owner.is_empty()
            || self.token.is_empty()
            || !self.ca_cert.is_absolute()
        {
            return Err(RelishError::InitFailed(
                "invalid local context schema, owner, token or CA path".to_string(),
            ));
        }
        super::client::validate_endpoint(&self.endpoint).map_err(|error| {
            RelishError::InitFailed(format!("invalid context endpoint: {error}"))
        })?;
        if !self.endpoint.starts_with("https://") {
            return Err(RelishError::InitFailed(
                "managed context requires HTTPS".to_string(),
            ));
        }
        Ok(())
    }
}

/// The default managed context path, separate from user shell configuration.
pub fn default_path() -> Result<PathBuf, RelishError> {
    Ok(root_directory()?.join("context.json"))
}

fn lock_context(path: &Path) -> Result<std::fs::File, RelishError> {
    let lock_path = path.with_extension("lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&lock_path)?;
    lock.try_lock()
        .map_err(|error| RelishError::InitFailed(format!("local context is busy: {error}")))?;
    // `relish uninstall` deletes the lock file (under the lock) once no
    // context is left. A writer that opened the file just before that
    // unlink now holds a lock nobody else can see, while a newer writer
    // could lock a fresh file at the same path. Refuse unless the file we
    // locked is still the one the path names.
    if !still_linked(&lock, &lock_path)? {
        return Err(RelishError::InitFailed(
            "local context is busy: its lock file was removed; retry".to_string(),
        ));
    }
    Ok(lock)
}

#[cfg(unix)]
fn still_linked(lock: &std::fs::File, lock_path: &Path) -> Result<bool, RelishError> {
    use std::os::unix::fs::MetadataExt;
    let held = lock.metadata()?;
    match std::fs::symlink_metadata(lock_path) {
        Ok(current) => Ok(current.dev() == held.dev() && current.ino() == held.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn still_linked(_lock: &std::fs::File, _lock_path: &Path) -> Result<bool, RelishError> {
    Ok(true)
}

/// Delete the context's lock file once no context remains beside it.
///
/// Returns `true` if the lock file was removed. The unlink happens while
/// holding the lock, and every locker re-checks that its file is still
/// linked, so no two writers can end up on different lock files. Leaves
/// everything alone (and returns `false`) while a context file exists.
pub fn remove_unused_lock(path: &Path) -> Result<bool, RelishError> {
    let lock_path = path.with_extension("lock");
    if std::fs::symlink_metadata(&lock_path).is_err() {
        return Ok(false);
    }
    let _lock = lock_context(path)?;
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(false);
    }
    std::fs::remove_file(&lock_path)?;
    Ok(true)
}

/// Managed local state root; an explicit override must be an absolute path.
pub fn root_directory() -> Result<PathBuf, RelishError> {
    if let Some(path) = std::env::var_os("RELIABURGER_HOME") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(RelishError::InitFailed(
                "RELIABURGER_HOME must be absolute".into(),
            ));
        }
        return Ok(path);
    }
    dirs::home_dir()
        .map(|home| home.join(".reliaburger"))
        .ok_or_else(|| {
            RelishError::InitFailed("cannot locate the home directory for local state".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_cluster_context_preserves_another_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "one").save(&path).unwrap();
        LocalContext::remove_owned(&path, "two").unwrap();
        assert!(path.exists());
        LocalContext::remove_owned(&path, "one").unwrap();
        assert!(!path.exists());
        LocalContext::remove_owned(&path, "one").unwrap();
    }

    #[test]
    fn unused_lock_is_removed_but_a_live_context_keeps_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let lock = root.path().join("context.lock");
        context(root.path(), "one").save(&path).unwrap();
        assert!(lock.exists(), "saving creates the lock beside the context");

        assert!(!remove_unused_lock(&path).unwrap());
        assert!(lock.exists(), "a saved context keeps its lock");

        LocalContext::remove_owned(&path, "one").unwrap();
        assert!(remove_unused_lock(&path).unwrap());
        assert!(!lock.exists());
        assert!(!remove_unused_lock(&path).unwrap(), "already gone");
    }

    #[test]
    fn unused_lock_is_left_while_another_process_holds_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let _held = lock_context(&path).unwrap();
        assert!(remove_unused_lock(&path).is_err());
        assert!(root.path().join("context.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_file_unlinked_or_replaced_is_detected() {
        let root = tempfile::tempdir().unwrap();
        let lock_path = root.path().join("context.lock");
        // A writer that opened the lock file before uninstall unlinked it.
        std::fs::write(&lock_path, "").unwrap();
        let stale = std::fs::File::open(&lock_path).unwrap();
        std::fs::remove_file(&lock_path).unwrap();
        assert!(!still_linked(&stale, &lock_path).unwrap());
        // A fresh file at the same path is a different inode.
        std::fs::write(&lock_path, "").unwrap();
        assert!(!still_linked(&stale, &lock_path).unwrap());
        let fresh = std::fs::File::open(&lock_path).unwrap();
        assert!(still_linked(&fresh, &lock_path).unwrap());
    }

    fn context(root: &std::path::Path, owner: &str) -> LocalContext {
        LocalContext {
            schema: 1,
            owner: owner.to_string(),
            endpoint: "https://127.0.0.1:19117".to_string(),
            token: "rbrg_private".to_string(),
            ca_cert: root.join("root-ca.crt"),
            service_endpoints: Default::default(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn saving_context_does_not_follow_a_preexisting_temporary_symlink() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let victim = root.path().join("unrelated.txt");
        std::fs::write(&victim, b"keep this file").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("tmp")).unwrap();
        context(root.path(), "cluster-a").save(&path).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep this file");
    }

    #[test]
    fn context_roundtrip_preserves_credentials_privately() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let original = context(root.path(), "cluster-a");
        original.save(&path).unwrap();
        let loaded = LocalContext::load(&path).unwrap().unwrap();
        assert_eq!(loaded.endpoint, original.endpoint);
        assert_eq!(loaded.token, original.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn setup_cannot_replace_another_clusters_context() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "cluster-a").save(&path).unwrap();
        assert!(context(root.path(), "cluster-b").save(&path).is_err());
        assert_eq!(
            LocalContext::load(&path).unwrap().unwrap().owner,
            "cluster-a"
        );
    }

    #[test]
    fn context_rejects_remote_plaintext_and_unknown_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let mut invalid = context(root.path(), "cluster-a");
        invalid.endpoint = "http://192.0.2.1:9117".to_string();
        assert!(invalid.save(&path).is_err());
        invalid.endpoint = "https://127.0.0.1:19117".to_string();
        invalid.schema = 2;
        assert!(invalid.save(&path).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn context_refuses_to_use_world_readable_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "cluster-a").save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(LocalContext::load(&path).is_err());
    }

    #[test]
    fn missing_context_leaves_the_ordinary_local_default_available() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            LocalContext::load(&root.path().join("absent"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forwards_apply_only_to_a_connection_pinned_to_the_same_ca() {
        let root = tempfile::tempdir().unwrap();
        let ca_cert = root.path().join("root-ca.crt");
        std::fs::write(&ca_cert, b"cluster ca").unwrap();
        let mut context = context(root.path(), "cluster-a");
        context.ca_cert = ca_cert;
        context.service_endpoints.registry = Some("https://127.0.0.1:15050".to_string());

        let forwards = context.forwards_for_ca(Some(b"cluster ca")).unwrap();
        assert_eq!(
            forwards.registry.as_deref(),
            Some("https://127.0.0.1:15050")
        );
        assert!(context.forwards_for_ca(Some(b"another ca")).is_none());
        assert!(context.forwards_for_ca(None).is_none());

        context.ca_cert = root.path().join("missing.crt");
        assert!(context.forwards_for_ca(Some(b"cluster ca")).is_none());
    }
}

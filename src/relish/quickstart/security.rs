//! Stable bootstrap credentials for a managed laptop cluster.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::relish::{RelishError, client::BunClient};
use crate::sesame::types::{ApiRole, CaRole, SecurityState, TokenScope};

use super::state::Operation;

/// Paths and public trust fingerprint for an operation's stable credentials.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    /// Owner-only directory containing identity, master key and administrator token.
    pub directory: PathBuf,
    /// Pinned root fingerprint used when enrolling additional nodes.
    pub root_fingerprint: String,
}

impl Bootstrap {
    /// Connect to the bootstrap API with its pinned CA and saved administrator bearer.
    pub fn client(&self, endpoint: &str) -> Result<BunClient, RelishError> {
        let token = private_read(&self.directory.join("admin.token"), 4096)?;
        let token =
            std::str::from_utf8(&token).map_err(|_| failed("invalid administrator token file"))?;
        let ca = std::fs::read(self.directory.join("identity/root-ca.crt"))?;
        BunClient::new_with_ca(endpoint, Some(token.trim()), &ca)
    }
}

/// Validate existing bootstrap credentials without generating or changing them.
pub fn existing(operation: &Operation) -> Result<Bootstrap, RelishError> {
    load(operation, &operation.directory.join("security"))
}

/// Generate credentials once, or validate and reuse the complete existing bundle.
/// Call this on the blocking pool before creating any VM.
pub fn prepare(operation: &Operation) -> Result<Bootstrap, RelishError> {
    let directory = operation.directory.join("security");
    if directory.exists() {
        return load(operation, &directory);
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("bootstrap-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let staging = builder.tempdir_in(&operation.directory)?;
    let first = operation
        .state
        .nodes
        .first()
        .ok_or_else(|| failed("cluster has no bootstrap node"))?;
    let mut init = crate::sesame::init::initialize_cluster(
        &operation.state.spec.name,
        &first.name,
        staging.path(),
    )
    .map_err(|error| failed(&format!("failed to generate bootstrap identity: {error}")))?;
    let administrator = crate::sesame::token::create_token(
        "laptop-admin",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .map_err(|error| failed(&format!("failed to create administrator token: {error}")))?;
    init.security_state.api_tokens.push(administrator.token);
    write_private(
        &staging.path().join("master.key"),
        hex::encode(init.master_secret).as_bytes(),
    )?;
    write_private(
        &staging.path().join("admin.token"),
        administrator.plaintext.as_bytes(),
    )?;
    let bootstrap =
        serde_json::to_vec_pretty(&init.security_state).map_err(RelishError::SerialiseJson)?;
    write_private(&staging.path().join("security-bootstrap.json"), &bootstrap)?;
    let identity = crate::relish::commands::node_identity_from_init(&init)?;
    crate::sesame::identity_store::save(&staging.path().join("identity"), &identity)
        .map_err(|error| failed(&format!("failed to persist bootstrap identity: {error}")))?;
    std::fs::File::open(&init.sealed_root_ca_path)?.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(staging.path())?.sync_all()?;
    std::fs::rename(staging.path(), &directory)?;
    let _ = staging.keep();
    #[cfg(unix)]
    std::fs::File::open(&operation.directory)?.sync_all()?;
    load(operation, &directory)
}

fn load(operation: &Operation, directory: &Path) -> Result<Bootstrap, RelishError> {
    let identity =
        crate::sesame::identity_store::load(&directory.join("identity")).map_err(|error| {
            failed(&format!(
                "existing bootstrap identity is invalid; refusing to regenerate it: {error}"
            ))
        })?;
    let identity = identity.ok_or_else(|| {
        failed("existing bootstrap identity is missing; refusing to regenerate it")
    })?;
    let first = operation
        .state
        .nodes
        .first()
        .ok_or_else(|| failed("cluster has no bootstrap node"))?;
    if identity.node_id != first.name {
        return Err(failed(
            "bootstrap identity belongs to a different cluster operation",
        ));
    }
    let master = private_read(&directory.join("master.key"), 128)?;
    let master =
        hex::decode(&master).map_err(|_| failed("invalid bootstrap master key encoding"))?;
    if master.len() != 32 {
        return Err(failed("invalid bootstrap master key length"));
    }
    let bytes = private_read(&directory.join("security-bootstrap.json"), 1024 * 1024)?;
    let state: SecurityState = serde_json::from_slice(&bytes)
        .map_err(|error| failed(&format!("invalid bootstrap state: {error}")))?;
    let node_ca = state
        .get_ca(CaRole::Node)
        .ok_or_else(|| failed("bootstrap node CA is missing"))?;
    let root_ca = state
        .get_ca(CaRole::Root)
        .ok_or_else(|| failed("bootstrap root CA is missing"))?;
    if node_ca.certificate_der != identity.node_ca_der
        || root_ca.certificate_der != identity.root_ca_der
    {
        return Err(failed(
            "bootstrap state and node identity have different CAs",
        ));
    }
    let wrapped = node_ca
        .private_key_wrapped
        .as_ref()
        .ok_or_else(|| failed("bootstrap node CA key is missing"))?;
    crate::sesame::crypto::unwrap_key(&master, wrapped)
        .map_err(|_| failed("bootstrap master key cannot unwrap the node CA"))?;
    let token = private_read(&directory.join("admin.token"), 4096)?;
    let token =
        std::str::from_utf8(&token).map_err(|_| failed("invalid administrator token encoding"))?;
    let principal = crate::sesame::auth::authenticate(token.trim(), &state.api_tokens)
        .map_err(|_| failed("administrator token does not match bootstrap state"))?;
    if principal.role != ApiRole::Admin {
        return Err(failed("bootstrap token is not an administrator"));
    }
    Ok(Bootstrap {
        directory: directory.to_path_buf(),
        root_fingerprint: crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der),
    })
}

fn private_read(path: &Path, maximum: u64) -> Result<Vec<u8>, RelishError> {
    let file = std::fs::File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err(failed(&format!(
                "private bootstrap file {} must have mode 0600",
                path.display()
            )));
        }
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(failed("bootstrap file exceeds its size limit"));
    }
    Ok(bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), RelishError> {
    crate::sesame::identity::atomic_write_mode(path, bytes, Some(0o600))?;
    Ok(())
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::state::{ClusterSpec, Operation};
    use super::*;

    fn operation(root: &std::path::Path) -> Operation {
        Operation::open(
            root,
            &ClusterSpec {
                name: "local".to_string(),
                nodes: 3,
                version: "v0.1.0".parse().unwrap(),
                api_port: 19117,
                ingress_port: 18080,
                registry_port: 15050,
            },
        )
        .unwrap()
    }

    #[test]
    fn retrying_setup_reuses_the_ca_master_key_and_administrator_token() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let first = prepare(&operation).unwrap();
        let master = std::fs::read(first.directory.join("master.key")).unwrap();
        let token = std::fs::read(first.directory.join("admin.token")).unwrap();
        let second = prepare(&operation).unwrap();
        assert_eq!(first.root_fingerprint, second.root_fingerprint);
        assert_eq!(
            std::fs::read(second.directory.join("master.key")).unwrap(),
            master
        );
        assert_eq!(
            std::fs::read(second.directory.join("admin.token")).unwrap(),
            token
        );
    }

    #[test]
    fn an_incomplete_existing_identity_is_refused_instead_of_regenerated() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let first = prepare(&operation).unwrap();
        let master = std::fs::read(first.directory.join("master.key")).unwrap();
        std::fs::remove_file(first.directory.join("identity/bundle.committed")).unwrap();
        assert!(prepare(&operation).is_err());
        assert_eq!(
            std::fs::read(first.directory.join("master.key")).unwrap(),
            master
        );
    }

    #[test]
    #[cfg(unix)]
    fn bootstrap_material_is_private_from_creation() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let bootstrap = prepare(&operation).unwrap();
        assert_eq!(
            std::fs::metadata(&bootstrap.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for name in [
            "master.key",
            "admin.token",
            "security-bootstrap.json",
            "identity/node.key",
            "identity/node.bundle.json",
        ] {
            assert_eq!(
                std::fs::metadata(bootstrap.directory.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

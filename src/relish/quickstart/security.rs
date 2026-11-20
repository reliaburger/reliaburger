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
    /// The root CA backup and the identity that opens it, once written.
    pub root_backup: Option<RootBackupFiles>,
}

/// The only copy of the root CA's key, on the operator's machine, and the
/// age identity that opens it, kept in another directory (#709). No node
/// ever gets either.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RootBackupFiles {
    /// The sealed backup, in the cluster's bootstrap directory.
    pub backup: PathBuf,
    /// The age identity that opens it.
    pub identity: PathBuf,
}

/// The backup's name inside a bootstrap directory.
pub const ROOT_BACKUP_FILE: &str = "root-ca-backup.age";

/// Where the identities that open root backups go, under relish's state
/// root (`~/.reliaburger`): apart from every cluster's bootstrap directory.
pub fn root_backup_keys_in(state_root: &Path) -> PathBuf {
    state_root.join("root-backup-keys")
}

/// Where a cluster's root backup and its identity go: the backup as
/// `backup` in `directory`, the identity in `keys`, named after the root.
pub(crate) fn root_backup_files_named(
    directory: &Path,
    backup: &str,
    keys: &Path,
    fingerprint: &str,
) -> RootBackupFiles {
    let hex = fingerprint.trim_start_matches("sha256:");
    RootBackupFiles {
        backup: directory.join(backup),
        identity: keys.join(format!("root-{}.age-identity", &hex[..hex.len().min(16)])),
    }
}

fn root_backup_files(directory: &Path, keys: &Path, fingerprint: &str) -> RootBackupFiles {
    root_backup_files_named(directory, ROOT_BACKUP_FILE, keys, fingerprint)
}

/// The keys directory for an operation in `<root>/clusters/<name>`.
fn operation_keys(operation: &Operation) -> Result<PathBuf, RelishError> {
    operation
        .directory
        .parent()
        .and_then(Path::parent)
        .map(root_backup_keys_in)
        .ok_or_else(|| failed("cluster directory has no state root"))
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
    load(
        &operation.directory.join("security"),
        first_node(operation)?,
        &operation_keys(operation)?,
    )
}

/// The name of a laptop quickstart cluster's administrator token, as
/// `relish token list` shows it.
pub const QUICKSTART_ADMINISTRATOR: &str = "laptop-admin";

/// Generate credentials once, or validate and reuse the complete existing bundle.
/// Call this on the blocking pool before creating any VM.
pub fn prepare(operation: &Operation) -> Result<Bootstrap, RelishError> {
    prepare_in(
        &operation.directory.join("security"),
        &operation.state.spec.name,
        first_node(operation)?,
        QUICKSTART_ADMINISTRATOR,
        &operation_keys(operation)?,
    )
}

fn first_node(operation: &Operation) -> Result<&str, RelishError> {
    operation
        .state
        .nodes
        .first()
        .map(|node| node.name.as_str())
        .ok_or_else(|| failed("cluster has no bootstrap node"))
}

/// Generate a cluster's credentials into `directory` once, or validate and
/// reuse the complete bundle already there.
///
/// The bundle holds the master key, the security bootstrap (with one
/// administrator token, named `administrator`), that token's plaintext and
/// the first node's identity, all owner-only. The name only matters to
/// people: it's what `relish token list` and the audit log show. `relish setup --quickstart` and
/// `relish dev create` both build their clusters from it. `directory` must
/// not exist yet or must hold a complete bundle for `first_node`; its parent
/// must exist.
///
/// The root CA's key goes only into `root-ca-backup.age` in the bundle,
/// sealed to a fresh age identity written into `keys` (#709). No node gets
/// either: [`node_security_files`] never names them.
pub fn prepare_in(
    directory: &Path,
    cluster: &str,
    first_node: &str,
    administrator: &str,
    keys: &Path,
) -> Result<Bootstrap, RelishError> {
    if directory.exists() {
        return load(directory, first_node, keys);
    }
    let parent = directory
        .parent()
        .ok_or_else(|| failed("bootstrap directory has no parent"))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("bootstrap-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let staging = builder.tempdir_in(parent)?;
    let mut init = crate::sesame::init::initialize_cluster(cluster, first_node)
        .map_err(|error| failed(&format!("failed to generate bootstrap identity: {error}")))?;
    create_private_dir(keys)?;
    let files = root_backup_files(directory, keys, &init.root_backup.fingerprint);
    crate::sesame::root_backup::write_backup(
        &init.root_backup,
        &crate::sesame::root_backup::BackupDestination::FreshIdentity {
            path: &staging.path().join(ROOT_BACKUP_FILE),
            identity: &files.identity,
        },
        std::time::SystemTime::now(),
    )?;
    let administrator = crate::sesame::token::create_token(
        administrator,
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
    #[cfg(unix)]
    std::fs::File::open(staging.path())?.sync_all()?;
    std::fs::rename(staging.path(), directory)?;
    let _ = staging.keep();
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    load(directory, first_node, keys)
}

pub(crate) fn create_private_dir(path: &Path) -> Result<(), RelishError> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The identity files of a bootstrap bundle, in install order: the commit
/// marker comes last, just as the local identity store writes it.
pub const IDENTITY_FILES: [&str; 7] = [
    "node.crt",
    "node.key",
    "node-ca.crt",
    "root-ca.crt",
    "meta.json",
    "node.bundle.json",
    "bundle.committed",
];

/// The private files node `index` needs from the bundle in `directory`, as
/// (host source, guest destination) pairs under `/etc/reliaburger`. Every
/// node gets the master key; only the first also gets the security
/// bootstrap and the first identity. The others enrol with `relish join`.
pub fn node_security_files(directory: &Path, index: usize) -> Vec<(PathBuf, String)> {
    let mut files = vec![(
        directory.join("master.key"),
        "/etc/reliaburger/master.key".to_string(),
    )];
    if index == 0 {
        files.push((
            directory.join("security-bootstrap.json"),
            "/etc/reliaburger/security-bootstrap.json".to_string(),
        ));
        for file in IDENTITY_FILES {
            files.push((
                directory.join("identity").join(file),
                format!("/etc/reliaburger/identity/{file}"),
            ));
        }
    }
    files
}

fn load(directory: &Path, first_node: &str, keys: &Path) -> Result<Bootstrap, RelishError> {
    let identity =
        crate::sesame::identity_store::load(&directory.join("identity")).map_err(|error| {
            failed(&format!(
                "existing bootstrap identity is invalid; refusing to regenerate it: {error}"
            ))
        })?;
    let identity = identity.ok_or_else(|| {
        failed("existing bootstrap identity is missing; refusing to regenerate it")
    })?;
    if identity.node_id != first_node {
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
        .active_ca(CaRole::Node)
        .ok_or_else(|| failed("bootstrap node CA is missing"))?;
    let root_ca = state
        .active_ca(CaRole::Root)
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
    let root_fingerprint =
        crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
    let files = root_backup_files(directory, keys, &root_fingerprint);
    Ok(Bootstrap {
        directory: directory.to_path_buf(),
        root_backup: files.backup.exists().then_some(files),
        root_fingerprint,
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
                ssh: Vec::new(),
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

    /// #709: the root key is only in the backup on this machine, which the
    /// identity in the keys directory opens, and no node is sent either.
    #[test]
    fn the_root_backup_stays_on_the_operators_machine_and_opens_with_its_identity() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let bootstrap = prepare(&operation).unwrap();
        let files = bootstrap.root_backup.clone().expect("a root backup");
        assert!(files.backup.starts_with(&bootstrap.directory));
        let state_root = operation.directory.parent().unwrap().parent().unwrap();
        assert!(
            files
                .identity
                .starts_with(state_root.join("root-backup-keys"))
        );
        assert!(!files.identity.starts_with(&bootstrap.directory));

        let identity = std::fs::read_to_string(&files.identity).unwrap();
        let identity: age::x25519::Identity = identity
            .lines()
            .find(|line| line.starts_with("AGE-SECRET-KEY-"))
            .unwrap()
            .parse()
            .unwrap();
        let backup = crate::sesame::root_backup::RootBackup::open(
            &std::fs::read(&files.backup).unwrap(),
            &crate::sesame::root_backup::BackupOpener::Identity(identity),
        )
        .unwrap();
        backup
            .verify(&bootstrap.root_fingerprint, std::time::SystemTime::now())
            .unwrap();

        for index in 0..3 {
            for (source, _) in node_security_files(&bootstrap.directory, index) {
                assert_ne!(source, files.backup);
                assert_ne!(source, files.identity);
            }
        }
        // A retry finds the same backup.
        assert_eq!(prepare(&operation).unwrap().root_backup, Some(files));
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

    fn token_names(bootstrap: &Bootstrap) -> Vec<String> {
        let bytes = std::fs::read(bootstrap.directory.join("security-bootstrap.json")).unwrap();
        let state: SecurityState = serde_json::from_slice(&bytes).unwrap();
        state
            .api_tokens
            .into_iter()
            .map(|token| token.name)
            .collect()
    }

    #[test]
    fn the_quickstart_administrator_token_is_named_laptop_admin() {
        let root = tempfile::tempdir().unwrap();
        let bootstrap = prepare(&operation(root.path())).unwrap();
        assert_eq!(token_names(&bootstrap), vec!["laptop-admin"]);
    }

    #[test]
    fn the_caller_names_the_administrator_token() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("security");
        let bootstrap = prepare_in(
            &directory,
            "lab",
            "lab-1",
            "dev-lab-admin",
            &root.path().join("keys"),
        )
        .unwrap();
        assert_eq!(token_names(&bootstrap), vec!["dev-lab-admin"]);
    }
}

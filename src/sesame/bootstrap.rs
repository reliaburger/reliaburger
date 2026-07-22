//! Loading the security bootstrap material a node needs at startup.
//!
//! `relish init` writes two secret files: `{cluster}-master.key` (the
//! hex-encoded 32-byte master secret) and `{cluster}-security-bootstrap.json`
//! (the initial [`SecurityState`]). This module reads them back on the node.
//!
//! Every clustered node loads the master key to build its wrapping IKM (used to
//! unwrap CA private keys and decrypt container secrets). Only the bootstrap
//! node loads the `SecurityState`, which it seeds into Raft once; from there it
//! replicates to the rest of the cluster.

use std::path::{Path, PathBuf};

use super::types::SecurityState;

/// Errors from loading bootstrap material off disk.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("failed to read {path:?}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("master key {path:?} is not valid hex: {source}")]
    InvalidHex {
        path: PathBuf,
        source: hex::FromHexError,
    },

    #[error("master key {path:?} must be 32 bytes, got {actual}")]
    WrongLength { path: PathBuf, actual: usize },

    #[error("failed to parse security bootstrap {path:?}: {source}")]
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },

    #[error("{path:?} is readable by group or other (mode {mode:#o}); expected 0600")]
    PermissionsTooOpen { path: PathBuf, mode: u32 },

    #[error(
        "{path:?} is owned by uid {owner}, not this process (uid {expected}); refusing to load"
    )]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },
}

/// Reject secret files that are readable beyond their owner.
///
/// A master key or bootstrap file left group/other-readable is a real leak, so
/// we fail closed rather than load it. No-op on non-Unix platforms.
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), BootstrapError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|source| BootstrapError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = meta.permissions().mode();
    // Any group (0o070) or other (0o007) permission bit set is too open.
    if mode & 0o077 != 0 {
        return Err(BootstrapError::PermissionsTooOpen {
            path: path.to_path_buf(),
            mode: mode & 0o777,
        });
    }
    // Ownership matters as much as mode (O15): a `0600` file owned by another
    // uid is that user's secret, not ours — mode `0600` on a file we don't own
    // means *they* can read it, and loading it as our master key trusts a file
    // an attacker placed. `root` (euid 0) may load any file it can already read.
    use std::os::unix::fs::MetadataExt;
    let owner = meta.uid();
    let euid = nix::unistd::geteuid().as_raw();
    if euid != 0 && owner != euid {
        return Err(BootstrapError::WrongOwner {
            path: path.to_path_buf(),
            owner,
            expected: euid,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<(), BootstrapError> {
    Ok(())
}

/// Load the hex-encoded 32-byte cluster master key.
pub fn load_master_key(path: &Path) -> Result<[u8; 32], BootstrapError> {
    check_permissions(path)?;
    let contents = std::fs::read_to_string(path).map_err(|source| BootstrapError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = hex::decode(contents.trim()).map_err(|source| BootstrapError::InvalidHex {
        path: path.to_path_buf(),
        source,
    })?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| BootstrapError::WrongLength {
            path: path.to_path_buf(),
            actual: bytes.len(),
        })
}

/// Load the initial [`SecurityState`] from the JSON bootstrap file.
pub fn load_bootstrap_state(path: &Path) -> Result<SecurityState, BootstrapError> {
    check_permissions(path)?;
    let contents = std::fs::read_to_string(path).map_err(|source| BootstrapError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| BootstrapError::InvalidJson {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write bytes to a temp file with owner-only (0600) permissions.
    fn write_secret(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rb-bootstrap-test-{name}"));
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn load_master_key_decodes_32_byte_hex() {
        let key = [7u8; 32];
        let path = write_secret("mk-round-trip", &hex::encode(key));
        let loaded = load_master_key(&path).unwrap();
        assert_eq!(loaded, key);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_master_key_rejects_wrong_length() {
        let path = write_secret("mk-short", &hex::encode([1u8; 16]));
        let err = load_master_key(&path).unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::WrongLength { actual: 16, .. }
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    #[cfg(unix)]
    fn load_master_key_rejects_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_secret("mk-open", &hex::encode([2u8; 32]));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_master_key(&path).unwrap_err();
        assert!(matches!(err, BootstrapError::PermissionsTooOpen { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_bootstrap_state_round_trips_security_state() {
        let dir = std::env::temp_dir().join("rb-bootstrap-test-state-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let init = crate::sesame::init::initialize_cluster("testcluster", "node-1", &dir).unwrap();
        let json = serde_json::to_string_pretty(&init.security_state).unwrap();
        let path = write_secret("state.json", &json);

        let loaded = load_bootstrap_state(&path).unwrap();
        // Four CAs (Root, Node, Workload, Ingress) and the cluster age key survive.
        assert_eq!(loaded.certificate_authorities.len(), 4);
        assert!(loaded.cluster_age_keypair().is_some());
        assert_eq!(
            loaded.cluster_age_keypair().unwrap().public_key,
            init.age_public_key
        );
        assert!(loaded.oidc_signing_config.is_some());

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}

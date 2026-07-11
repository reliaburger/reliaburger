//! On-disk persistence for the node's cluster identity.
//!
//! A node's identity is the certificate the Node CA issued for it plus the
//! CA certificates needed to verify peers. `relish init` writes the first
//! node's identity here; joiners receive theirs through the join ceremony.
//! Bun loads the identity at startup to enable mTLS on its listeners.
//!
//! Layout under the identity directory (default `{storage.data}/identity`):
//!
//! ```text
//! node.crt      # this node's certificate (PEM)
//! node.key      # this node's private key (PEM, owner-only)
//! node-ca.crt   # the Node CA that signed node.crt (PEM)
//! root-ca.crt   # the cluster root CA — the trust anchor (PEM)
//! meta.json     # node_id, serial, ca_generation, validity window
//! ```

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::cert;
use super::identity::{atomic_write, atomic_write_mode};
use super::types::SerialNumber;

/// Errors from reading or writing the identity directory.
#[derive(Debug, thiserror::Error)]
pub enum IdentityStoreError {
    #[error("identity io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse {file}: {reason}")]
    ParseFailed { file: String, reason: String },
}

/// A node's cluster identity: its certificate and private key, plus the CA
/// certificates a peer verifier needs.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    /// The node's unique identifier (the certificate's CN).
    pub node_id: String,
    /// DER-encoded node certificate, signed by the Node CA.
    pub certificate_der: Vec<u8>,
    /// DER-encoded private key. Never leaves this node's disk.
    pub private_key_der: Vec<u8>,
    /// Serial number the Node CA assigned.
    pub serial: SerialNumber,
    /// The Node CA generation that signed this certificate.
    pub ca_generation: u64,
    /// DER-encoded Node CA certificate (presented as the chain intermediate).
    pub node_ca_der: Vec<u8>,
    /// DER-encoded root CA certificate (the trust anchor for peer checks).
    pub root_ca_der: Vec<u8>,
    /// Certificate validity start.
    pub not_before: SystemTime,
    /// Certificate validity end.
    pub not_after: SystemTime,
}

/// Sidecar metadata persisted beside the PEM files.
#[derive(Debug, Serialize, Deserialize)]
struct IdentityMeta {
    node_id: String,
    serial: SerialNumber,
    ca_generation: u64,
    not_before: SystemTime,
    not_after: SystemTime,
}

const NODE_CERT_FILE: &str = "node.crt";
const NODE_KEY_FILE: &str = "node.key";
const NODE_CA_FILE: &str = "node-ca.crt";
const ROOT_CA_FILE: &str = "root-ca.crt";
const META_FILE: &str = "meta.json";

/// The identity directory under a node's data directory.
pub fn identity_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("identity")
}

/// Persist a node identity to `dir`, creating the directory if needed.
///
/// The private key is written owner-only (0600); certificates are public
/// material and stay world-readable. All writes are atomic (temp + rename)
/// so a crash never leaves a truncated key or certificate at the final path.
pub fn save(dir: &Path, identity: &NodeIdentity) -> Result<(), IdentityStoreError> {
    std::fs::create_dir_all(dir)?;

    let cert_pem = cert::der_to_pem(&identity.certificate_der, "CERTIFICATE");
    let key_pem = cert::der_to_pem(&identity.private_key_der, "PRIVATE KEY");
    let node_ca_pem = cert::der_to_pem(&identity.node_ca_der, "CERTIFICATE");
    let root_ca_pem = cert::der_to_pem(&identity.root_ca_der, "CERTIFICATE");

    let meta = IdentityMeta {
        node_id: identity.node_id.clone(),
        serial: identity.serial,
        ca_generation: identity.ca_generation,
        not_before: identity.not_before,
        not_after: identity.not_after,
    };
    let meta_json =
        serde_json::to_string_pretty(&meta).map_err(|e| IdentityStoreError::ParseFailed {
            file: META_FILE.to_string(),
            reason: e.to_string(),
        })?;

    atomic_write(&dir.join(NODE_CERT_FILE), cert_pem.as_bytes())?;
    atomic_write_mode(&dir.join(NODE_KEY_FILE), key_pem.as_bytes(), Some(0o600))?;
    atomic_write(&dir.join(NODE_CA_FILE), node_ca_pem.as_bytes())?;
    atomic_write(&dir.join(ROOT_CA_FILE), root_ca_pem.as_bytes())?;
    atomic_write(&dir.join(META_FILE), meta_json.as_bytes())?;

    Ok(())
}

/// Load a node identity from `dir`.
///
/// Returns `Ok(None)` when no identity has been installed (the directory or
/// its metadata file is absent) — callers treat that as "plaintext mode" or
/// "enrollment pending", not an error.
pub fn load(dir: &Path) -> Result<Option<NodeIdentity>, IdentityStoreError> {
    let meta_path = dir.join(META_FILE);
    if !meta_path.exists() {
        return Ok(None);
    }

    let meta_json = std::fs::read_to_string(&meta_path)?;
    let meta: IdentityMeta =
        serde_json::from_str(&meta_json).map_err(|e| IdentityStoreError::ParseFailed {
            file: META_FILE.to_string(),
            reason: e.to_string(),
        })?;

    let certificate_der = read_pem(dir, NODE_CERT_FILE)?;
    let private_key_der = read_pem(dir, NODE_KEY_FILE)?;
    let node_ca_der = read_pem(dir, NODE_CA_FILE)?;
    let root_ca_der = read_pem(dir, ROOT_CA_FILE)?;

    Ok(Some(NodeIdentity {
        node_id: meta.node_id,
        certificate_der,
        private_key_der,
        serial: meta.serial,
        ca_generation: meta.ca_generation,
        node_ca_der,
        root_ca_der,
        not_before: meta.not_before,
        not_after: meta.not_after,
    }))
}

/// A `sha256:HEX` fingerprint of a DER certificate.
///
/// Shown by `relish init` and `relish join` so an operator can compare the
/// root CA a joiner received against the one the cluster was created with.
pub fn root_ca_fingerprint(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    format!("sha256:{}", hex::encode(digest.as_ref()))
}

fn read_pem(dir: &Path, file: &str) -> Result<Vec<u8>, IdentityStoreError> {
    let content = std::fs::read_to_string(dir.join(file))?;
    let parsed = ::pem::parse(&content).map_err(|e| IdentityStoreError::ParseFailed {
        file: file.to_string(),
        reason: e.to_string(),
    })?;
    Ok(parsed.contents().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca;

    fn test_identity() -> NodeIdentity {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (cert_der, key_der, serial) = ca::issue_node_cert(
            "node-01",
            SerialNumber(5),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        NodeIdentity {
            node_id: "node-01".to_string(),
            certificate_der: cert_der,
            private_key_der: key_der,
            serial,
            ca_generation: 0,
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + std::time::Duration::from_secs(365 * 24 * 3600),
        }
    }

    #[test]
    fn identity_store_round_trips_a_node_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity();

        save(dir.path(), &identity).unwrap();
        let loaded = load(dir.path()).unwrap().expect("identity should load");

        assert_eq!(loaded.node_id, identity.node_id);
        assert_eq!(loaded.certificate_der, identity.certificate_der);
        assert_eq!(loaded.private_key_der, identity.private_key_der);
        assert_eq!(loaded.serial, identity.serial);
        assert_eq!(loaded.ca_generation, identity.ca_generation);
        assert_eq!(loaded.node_ca_der, identity.node_ca_der);
        assert_eq!(loaded.root_ca_der, identity.root_ca_der);
    }

    #[test]
    fn identity_store_load_returns_none_when_no_identity_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-identity");
        assert!(load(&missing).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn identity_store_writes_the_private_key_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();

        let mode = std::fs::metadata(dir.path().join(NODE_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn root_ca_fingerprint_is_a_prefixed_sha256_hex_digest() {
        let identity = test_identity();
        let fp = root_ca_fingerprint(&identity.root_ca_der);
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), "sha256:".len() + 64);
        // Deterministic for the same DER
        assert_eq!(fp, root_ca_fingerprint(&identity.root_ca_der));
    }

    #[test]
    fn loaded_identity_still_validates_against_its_chain() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();

        crate::sesame::cert::validate_chain(
            &loaded.certificate_der,
            &loaded.node_ca_der,
            &loaded.root_ca_der,
        )
        .unwrap();
    }
}

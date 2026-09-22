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
//! meta.json     # exported node_id, serial, ca_generation, validity window
//! node.bundle.json # authoritative complete identity (owner-only)
//! bundle.committed # layout version, written last on initial installation
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
    #[error("identity bundle is incomplete: no commit marker (interrupted install)")]
    PartialBundle,
    #[error("identity bundle is inconsistent: {reason}")]
    InconsistentBundle { reason: String },
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

/// The complete identity is one private atomic replacement, never a mixture
/// of separately replaced PEM files. PEM files remain compatibility exports.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityBundle {
    schema: u32,
    meta: IdentityMeta,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    node_ca_der: Vec<u8>,
    root_ca_der: Vec<u8>,
}

impl IdentityBundle {
    fn from_identity(identity: &NodeIdentity) -> Self {
        Self {
            schema: 2,
            meta: IdentityMeta {
                node_id: identity.node_id.clone(),
                serial: identity.serial,
                ca_generation: identity.ca_generation,
                not_before: identity.not_before,
                not_after: identity.not_after,
            },
            certificate_der: identity.certificate_der.clone(),
            private_key_der: identity.private_key_der.clone(),
            node_ca_der: identity.node_ca_der.clone(),
            root_ca_der: identity.root_ca_der.clone(),
        }
    }

    fn into_identity(self) -> Result<NodeIdentity, IdentityStoreError> {
        if self.schema != 2 {
            return Err(inconsistent("unsupported identity snapshot schema"));
        }
        validate_identity(NodeIdentity {
            node_id: self.meta.node_id,
            serial: self.meta.serial,
            ca_generation: self.meta.ca_generation,
            not_before: self.meta.not_before,
            not_after: self.meta.not_after,
            certificate_der: self.certificate_der,
            private_key_der: self.private_key_der,
            node_ca_der: self.node_ca_der,
            root_ca_der: self.root_ca_der,
        })
    }
}

const BUNDLE_FILE: &str = "node.bundle.json";

const NODE_CERT_FILE: &str = "node.crt";
const NODE_KEY_FILE: &str = "node.key";
const NODE_CA_FILE: &str = "node-ca.crt";
const ROOT_CA_FILE: &str = "root-ca.crt";
const META_FILE: &str = "meta.json";
/// Written last, after every other file lands (PKI9). Its presence marks the
/// bundle as complete; `load` refuses a directory without it, so a crash
/// mid-install (some files written, marker not) is detected rather than a
/// half-installed identity being used.
const COMMIT_MARKER_FILE: &str = "bundle.committed";

/// The identity directory under a node's data directory.
pub fn identity_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("identity")
}

/// Persist a node identity to `dir`, creating the directory if needed.
///
/// The private key is written owner-only (0600); certificates are public
/// material and stay world-readable. Each write is atomic (temp + rename) so
/// no individual file is ever truncated.
///
/// A complete private snapshot is the source of truth (PKI9). Initial install
/// writes its layout marker last. Replacement retains the previous snapshot
/// and marker until the new snapshot is durable; failed export writes leave the
/// old identity loadable. Legacy layout 1 is validated and snapshotted before
/// touching its exports. Readers never fall back from a broken layout 2 snapshot.
pub fn save(dir: &Path, identity: &NodeIdentity) -> Result<(), IdentityStoreError> {
    let identity = validate_identity(identity.clone())?;
    std::fs::create_dir_all(dir)?;
    let marker_path = dir.join(COMMIT_MARKER_FILE);
    let marker = match std::fs::read(&marker_path) {
        Ok(marker) => Some(marker),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    match marker.as_deref() {
        Some(b"1\n") => {
            let previous = load(dir)?.ok_or(IdentityStoreError::PartialBundle)?;
            write_snapshot(dir, &previous)?;
            atomic_write(&marker_path, b"2\n")?;
        }
        Some(b"2\n") => {
            if !dir.join(BUNDLE_FILE).is_file() {
                return Err(inconsistent("committed identity snapshot is missing"));
            }
        }
        Some(_) => return Err(inconsistent("unsupported identity layout")),
        None if has_identity_files(dir) => {
            return Err(IdentityStoreError::PartialBundle);
        }
        None => {}
    }

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

    write_snapshot(dir, &identity)?;
    if marker.is_none() {
        // A fresh install is visible only after its complete snapshot exists.
        atomic_write(&marker_path, b"2\n")?;
    }

    Ok(())
}

/// Load a node identity from `dir`.
///
/// Returns `Ok(None)` only when no identity material has been installed.
/// Any known identity file without the commit marker is an incomplete install,
/// never an invitation to fall back to plaintext or enrolment-pending mode.
///
/// A partial installation is refused. Layout 2 reads only the complete private
/// snapshot, so interrupted PEM export replacement cannot tear a live identity.
/// Layout 1 remains readable for deliberate import into the atomic layout.
pub fn load(dir: &Path) -> Result<Option<NodeIdentity>, IdentityStoreError> {
    let meta_path = dir.join(META_FILE);
    let marker = match std::fs::read(dir.join(COMMIT_MARKER_FILE)) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if has_identity_files(dir) {
                return Err(IdentityStoreError::PartialBundle);
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    match marker.as_slice() {
        b"2\n" => {
            let file = std::fs::File::open(dir.join(BUNDLE_FILE))?;
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err(inconsistent("identity snapshot exceeds 1 MiB"));
            }
            let bundle: IdentityBundle = serde_json::from_slice(&bytes).map_err(|error| {
                IdentityStoreError::ParseFailed {
                    file: BUNDLE_FILE.into(),
                    // Serde type errors can quote input; this file contains a key.
                    reason: format!(
                        "invalid identity JSON at line {} column {}",
                        error.line(),
                        error.column()
                    ),
                }
            })?;
            return bundle.into_identity().map(Some);
        }
        b"1\n" => {}
        _ => return Err(inconsistent("unsupported identity layout")),
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

    validate_identity(NodeIdentity {
        node_id: meta.node_id,
        certificate_der,
        private_key_der,
        serial: meta.serial,
        ca_generation: meta.ca_generation,
        node_ca_der,
        root_ca_der,
        not_before: meta.not_before,
        not_after: meta.not_after,
    })
    .map(Some)
}

fn has_identity_files(dir: &Path) -> bool {
    [
        NODE_CERT_FILE,
        NODE_KEY_FILE,
        NODE_CA_FILE,
        ROOT_CA_FILE,
        META_FILE,
        BUNDLE_FILE,
    ]
    .iter()
    .any(|file| dir.join(file).exists())
}

fn inconsistent(reason: impl Into<String>) -> IdentityStoreError {
    IdentityStoreError::InconsistentBundle {
        reason: reason.into(),
    }
}

fn write_snapshot(dir: &Path, identity: &NodeIdentity) -> Result<(), IdentityStoreError> {
    let bytes = serde_json::to_vec(&IdentityBundle::from_identity(identity))
        .map_err(|_| inconsistent("failed to serialise identity snapshot"))?;
    atomic_write_mode(&dir.join(BUNDLE_FILE), &bytes, Some(0o600))?;
    Ok(())
}

/// Validate a complete identity and derive its validity from the signed leaf.
pub(crate) fn validate_identity(
    mut identity: NodeIdentity,
) -> Result<NodeIdentity, IdentityStoreError> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use x509_parser::prelude::FromDer;
    cert::validate_chain(
        &identity.certificate_der,
        &identity.node_ca_der,
        &identity.root_ca_der,
    )
    .map_err(|error| inconsistent(error.to_string()))?;
    cert::check_issuer_binding(&identity.certificate_der, &identity.node_ca_der)
        .and_then(|()| cert::check_issuer_binding(&identity.node_ca_der, &identity.root_ca_der))
        .map_err(|error| inconsistent(error.to_string()))?;
    let (_, leaf) = x509_parser::certificate::X509Certificate::from_der(&identity.certificate_der)
        .map_err(|_| inconsistent("invalid node certificate"))?;
    let san = leaf
        .subject_alternative_name()
        .map_err(|_| inconsistent("invalid node SAN"))?
        .ok_or_else(|| inconsistent("missing node SAN"))?;
    let node_ids: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|name| {
            if let x509_parser::extensions::GeneralName::URI(uri) = name {
                super::ca::node_id_from_spiffe_uri(uri)
            } else {
                None
            }
        })
        .collect();
    if node_ids.as_slice() != [identity.node_id.as_str()] {
        return Err(inconsistent(
            "node metadata does not match the signed node identity",
        ));
    }
    let serial = leaf.serial.to_bytes_be();
    if serial.len() > 8
        || serial
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
            != identity.serial.0
    {
        return Err(inconsistent(
            "node metadata does not match the signed serial",
        ));
    }
    let key = PrivateKeyDer::try_from(identity.private_key_der.clone())
        .map_err(|_| inconsistent("invalid node private key"))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|_| inconsistent("unsupported node private key"))?;
    rustls::sign::CertifiedKey::new(
        vec![CertificateDer::from(identity.certificate_der.clone())],
        signing_key,
    )
    .keys_match()
    .map_err(|_| inconsistent("node private key does not match its certificate"))?;
    let system_time = |timestamp: i64| -> Result<SystemTime, IdentityStoreError> {
        let seconds = u64::try_from(timestamp)
            .map_err(|_| inconsistent("node validity predates the Unix epoch"))?;
        SystemTime::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(seconds))
            .ok_or_else(|| inconsistent("node validity exceeds the supported clock range"))
    };
    identity.not_before = system_time(leaf.validity().not_before.timestamp())?;
    identity.not_after = system_time(leaf.validity().not_after.timestamp())?;
    Ok(identity)
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

        for file in [NODE_KEY_FILE, BUNDLE_FILE] {
            let mode = std::fs::metadata(dir.path().join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn load_refuses_a_bundle_missing_the_commit_marker() {
        // PKI9: simulate a crash after the metadata landed but before the
        // marker. load must refuse, not run on a half-installed identity.
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();
        std::fs::remove_file(dir.path().join(COMMIT_MARKER_FILE)).unwrap();

        let err = load(dir.path()).unwrap_err();
        assert!(matches!(err, IdentityStoreError::PartialBundle));
    }

    #[test]
    fn load_refuses_a_legacy_bundle_missing_a_file_even_with_a_marker() {
        // A marker present but a file gone is still a broken bundle; the
        // missing PEM read errors out rather than yielding a partial identity.
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();
        std::fs::write(dir.path().join(COMMIT_MARKER_FILE), b"1\n").unwrap();
        std::fs::remove_file(dir.path().join(BUNDLE_FILE)).unwrap();
        std::fs::remove_file(dir.path().join(NODE_CERT_FILE)).unwrap();

        assert!(load(dir.path()).is_err());
    }

    #[test]
    fn save_refuses_a_bundle_whose_leaf_does_not_chain() {
        // Reject a foreign CA chain before installing any identity files.
        let dir = tempfile::tempdir().unwrap();
        let mut identity = test_identity();
        let foreign = ca::generate_ca_hierarchy("intruder", b"other").unwrap();
        identity.node_ca_der = foreign.node.ca.certificate_der.clone();
        identity.root_ca_der = foreign.root.ca.certificate_der.clone();
        let err = save(dir.path(), &identity).unwrap_err();
        assert!(matches!(err, IdentityStoreError::InconsistentBundle { .. }));
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_committed_bundle_loads_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();
        assert!(load(dir.path()).unwrap().is_some());
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
    #[test]
    fn failed_replacement_preserves_the_previous_complete_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity();
        save(dir.path(), &identity).unwrap();
        // Force an export write to fail after an earlier file has been replaced.
        std::fs::remove_file(dir.path().join(NODE_KEY_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(NODE_KEY_FILE)).unwrap();
        assert!(save(dir.path(), &identity).is_err());
        let loaded = load(dir.path())
            .expect("failed renewal must preserve the committed identity")
            .unwrap();
        assert_eq!(loaded.certificate_der, identity.certificate_der);
        assert_eq!(loaded.private_key_der, identity.private_key_der);
    }

    #[test]
    fn a_mismatched_replacement_key_is_refused_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity();
        save(dir.path(), &identity).unwrap();
        let mut replacement = identity.clone();
        replacement.private_key_der = test_identity().private_key_der;
        assert!(save(dir.path(), &replacement).is_err());
        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.private_key_der, identity.private_key_der);
    }

    #[test]
    fn identity_metadata_cannot_change_the_signed_node_or_serial() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity();
        save(dir.path(), &identity).unwrap();
        let mut replacement = identity.clone();
        replacement.node_id = "another-node".into();
        assert!(save(dir.path(), &replacement).is_err());
        replacement = identity.clone();
        replacement.serial = SerialNumber(999);
        assert!(save(dir.path(), &replacement).is_err());
        assert_eq!(load(dir.path()).unwrap().unwrap().serial, identity.serial);
    }

    #[test]
    fn loaded_validity_comes_from_the_certificate_not_sidecar_timestamps() {
        use x509_parser::prelude::FromDer;
        let dir = tempfile::tempdir().unwrap();
        let mut identity = test_identity();
        identity.not_before = SystemTime::UNIX_EPOCH;
        identity.not_after = SystemTime::UNIX_EPOCH;
        save(dir.path(), &identity).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();
        let (_, certificate) =
            x509_parser::certificate::X509Certificate::from_der(&identity.certificate_der).unwrap();
        assert_eq!(
            loaded
                .not_before
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            certificate.validity().not_before.timestamp() as u64
        );
        assert_eq!(
            loaded
                .not_after
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            certificate.validity().not_after.timestamp() as u64
        );
    }
    #[test]
    fn missing_or_corrupt_snapshot_never_falls_back_to_exports() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &test_identity()).unwrap();
        let path = dir.path().join(BUNDLE_FILE);
        let mut bundle: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        bundle["private_key_der"] = serde_json::json!("sensitive-placeholder");
        std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
        let error = load(dir.path()).unwrap_err().to_string();
        assert!(
            !error.contains("sensitive-placeholder"),
            "parse errors must not echo key material"
        );
        std::fs::remove_file(path).unwrap();
        assert!(load(dir.path()).is_err());
    }

    #[test]
    fn validated_legacy_identity_is_imported_before_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let identity = test_identity();
        save(dir.path(), &identity).unwrap();
        std::fs::remove_file(dir.path().join(BUNDLE_FILE)).unwrap();
        std::fs::write(dir.path().join(COMMIT_MARKER_FILE), b"1\n").unwrap();
        assert_eq!(
            load(dir.path()).unwrap().unwrap().certificate_der,
            identity.certificate_der
        );
        save(dir.path(), &identity).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(COMMIT_MARKER_FILE)).unwrap(),
            b"2\n"
        );
        assert!(dir.path().join(BUNDLE_FILE).is_file());
        assert_eq!(
            load(dir.path()).unwrap().unwrap().certificate_der,
            identity.certificate_der
        );
    }

    #[test]
    fn readers_observe_complete_identities_during_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let first = test_identity();
        let second = test_identity();
        save(dir.path(), &first).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..100 {
                    let loaded = load(dir.path()).unwrap().unwrap();
                    assert!(
                        loaded.certificate_der == first.certificate_der
                            || loaded.certificate_der == second.certificate_der
                    );
                }
            });
            barrier.wait();
            for index in 0..20 {
                save(dir.path(), if index % 2 == 0 { &second } else { &first }).unwrap();
            }
        });
    }
    #[test]
    fn partial_initial_pem_material_never_looks_unenrolled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(NODE_CERT_FILE),
            b"interrupted initial installation",
        )
        .unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(IdentityStoreError::PartialBundle)
        ));
    }
}

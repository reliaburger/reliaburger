//! Encrypted external council backup (Phase 12b.2, findings D21/CP12).
//!
//! The whitepaper promises the cluster's desired state is reconstructable.
//! Self-healing (H2/D2) keeps the council alive while a *majority* survives,
//! but if every voter dies at once there is nothing left to elect from. The
//! answer is a leader-only periodic export of the state-machine snapshot to
//! external storage, sealed so the blob is useless without the cluster
//! master key.
//!
//! The payload is the state machine's JSON `DesiredState` (the same bytes the
//! #83 snapshot envelope carries, version and checksum already inside). We
//! seal it with a key derived from the cluster master key via HKDF, using a
//! backup-specific info string so the wrapping key can't be confused with any
//! other derived key (CA wrapping, secrets). Every node holds the master key
//! under `/etc/reliaburger`, so any survivor can restore.
//!
//! Upload rides `object_store`, exactly like `bun::snapshot_worker`: one
//! interface over `file://`, `s3://` and `gs://`, credentials from each
//! backend's standard environment variables.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::council::node::CouncilNode;
use crate::sesame::crypto::{self, CryptoError};

/// HKDF info string that scopes the backup-sealing key. Binding the derived
/// key to this purpose prevents a sealed backup from being decrypted with, or
/// mistaken for, any other key derived from the same master secret.
const BACKUP_SEAL_INFO: &str = "reliaburger-council-backup-seal-v1";

/// Format version of the sealed-backup envelope. Bump when the on-disk shape
/// changes so an older binary refuses a newer blob rather than mis-parsing it.
const BACKUP_ENVELOPE_VERSION: u32 = 1;

/// Configuration for the council backup loop (`[cluster.backup]`).
///
/// Off by default: `url` unset means the loop never starts, so a cluster that
/// doesn't want external backups pays nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// Object-store destination (`file://`, `s3://`, `gs://`). `None`
    /// disables backups entirely.
    pub url: Option<String>,
    /// Seconds between backup exports. Clamped to at least 1 by the loop.
    pub interval_secs: u64,
    /// Sealed backups retained at the destination; older ones are pruned.
    pub retain: usize,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            url: None,
            interval_secs: 300,
            retain: 24,
        }
    }
}

impl BackupConfig {
    /// Whether the backup loop should run (a destination is configured).
    pub fn enabled(&self) -> bool {
        self.url.is_some()
    }

    /// Validate the backup settings. Only checked when backups are enabled:
    /// a positive interval (a zero interval would busy-loop) and at least one
    /// retained backup (retaining zero would delete every backup the moment
    /// it's uploaded, defeating the point).
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        if self.interval_secs == 0 {
            return Err("cluster.backup.interval_secs must be greater than zero".to_string());
        }
        if self.retain == 0 {
            return Err("cluster.backup.retain must be at least 1".to_string());
        }
        Ok(())
    }
}

/// A sealed council backup: the AES-256-GCM ciphertext of the snapshot
/// payload plus everything a survivor needs to derive the same wrapping key
/// and verify integrity. Self-describing JSON, so a restore never has to
/// guess the format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedBackup {
    /// Envelope format version.
    pub version: u32,
    /// HKDF salt used to derive the wrapping key from the master key.
    pub hkdf_salt: [u8; 32],
    /// HKDF info string (recorded for transparency and forward-compatibility).
    pub hkdf_info: String,
    /// AES-256-GCM nonce.
    pub nonce: [u8; 12],
    /// Ciphertext with the 16-byte GCM tag appended.
    pub ciphertext: Vec<u8>,
}

/// Errors from sealing, unsealing, or shipping a council backup.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error(
        "backup envelope version {found} is not supported (this binary supports up to {supported})"
    )]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("serialise backup: {0}")]
    Serialise(String),
    #[error("invalid backup url {url}: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("object store: {0}")]
    ObjectStore(String),
}

/// Seal `payload` (the snapshot JSON bytes) with a key derived from
/// `master_key`. The returned envelope is safe to store anywhere: without the
/// master key it is opaque, and any tampering fails the GCM tag on unseal.
pub fn seal_snapshot(
    master_key: &[u8; 32],
    payload: &[u8],
    _config: &BackupConfig,
) -> Result<SealedBackup, BackupError> {
    // A fresh random salt per backup: the derived wrapping key differs even
    // for identical state, and the salt travels in the (public) envelope.
    let salt = crypto::random_bytes::<32>()?;
    let wrapping_key = crypto::hkdf_derive_key(master_key, &salt, BACKUP_SEAL_INFO)?;
    let (ciphertext, nonce) = crypto::aes_256_gcm_encrypt(&wrapping_key, payload)?;
    Ok(SealedBackup {
        version: BACKUP_ENVELOPE_VERSION,
        hkdf_info: BACKUP_SEAL_INFO.to_string(),
        nonce,
        ciphertext,
        hkdf_salt: salt,
    })
}

/// Unseal a backup produced by [`seal_snapshot`], returning the original
/// payload bytes. Fails if the version is unknown, the master key is wrong, or
/// the ciphertext was tampered with (the GCM tag won't verify).
pub fn unseal_snapshot(
    master_key: &[u8; 32],
    sealed: &SealedBackup,
) -> Result<Vec<u8>, BackupError> {
    if sealed.version != BACKUP_ENVELOPE_VERSION {
        return Err(BackupError::UnsupportedVersion {
            found: sealed.version,
            supported: BACKUP_ENVELOPE_VERSION,
        });
    }
    let wrapping_key = crypto::hkdf_derive_key(master_key, &sealed.hkdf_salt, &sealed.hkdf_info)?;
    let plaintext = crypto::aes_256_gcm_decrypt(&wrapping_key, &sealed.ciphertext, &sealed.nonce)?;
    Ok(plaintext)
}

/// Serialise a sealed backup to the bytes that get uploaded.
pub fn encode_backup(sealed: &SealedBackup) -> Result<Vec<u8>, BackupError> {
    serde_json::to_vec(sealed).map_err(|e| BackupError::Serialise(e.to_string()))
}

/// Parse the bytes of a stored backup back into a [`SealedBackup`].
pub fn decode_backup(bytes: &[u8]) -> Result<SealedBackup, BackupError> {
    serde_json::from_slice(bytes).map_err(|e| BackupError::Serialise(e.to_string()))
}

/// An object-store destination for sealed backups, built from a URL.
pub struct BackupStore {
    store: Box<dyn object_store::ObjectStore>,
    prefix: object_store::path::Path,
}

impl BackupStore {
    /// Open the store behind `url` (`file://`, `s3://`, `gs://`).
    pub fn from_url(url: &str) -> Result<Self, BackupError> {
        let parsed = url::Url::parse(url).map_err(|e| BackupError::InvalidUrl {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
        let (store, prefix) =
            object_store::parse_url(&parsed).map_err(|e| BackupError::InvalidUrl {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self { store, prefix })
    }

    /// Object key for the backup taken at `now`. Millisecond-precision epoch
    /// timestamps sort lexicographically in creation order, which is exactly
    /// what the retention prune relies on.
    fn key_for(&self, now: SystemTime) -> object_store::path::Path {
        let millis = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        // Zero-pad so string order matches time order for the next ~300 years.
        self.prefix.child(format!("council-{millis:016}.backup"))
    }

    /// Upload one sealed backup.
    pub async fn put(&self, sealed: &SealedBackup, now: SystemTime) -> Result<(), BackupError> {
        let bytes = encode_backup(sealed)?;
        self.store
            .put(&self.key_for(now), object_store::PutPayload::from(bytes))
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        Ok(())
    }

    /// List stored backup keys, oldest first (lexicographic == chronological).
    pub async fn list_keys(&self) -> Result<Vec<object_store::path::Path>, BackupError> {
        use futures_util::StreamExt;
        let mut keys: Vec<object_store::path::Path> = Vec::new();
        let mut stream = self.store.list(Some(&self.prefix));
        while let Some(entry) = stream.next().await {
            let meta = entry.map_err(|e| BackupError::ObjectStore(e.to_string()))?;
            if meta
                .location
                .filename()
                .is_some_and(|f| f.ends_with(".backup"))
            {
                keys.push(meta.location);
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// Fetch and decode the newest stored backup, `None` if the destination
    /// holds none. Used by the recovery path when the operator points at a
    /// backup URL rather than a local data directory.
    pub async fn latest(&self) -> Result<Option<SealedBackup>, BackupError> {
        let keys = self.list_keys().await?;
        let Some(newest) = keys.last() else {
            return Ok(None);
        };
        let result = self
            .store
            .get(newest)
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        Ok(Some(decode_backup(&bytes)?))
    }

    /// Prune every backup beyond the newest `retain`. Pure decision, real
    /// deletes: returns how many were removed.
    pub async fn prune(&self, retain: usize) -> Result<usize, BackupError> {
        let keys = self.list_keys().await?;
        let doomed = prune_plan(&keys, retain);
        let mut removed = 0;
        for key in doomed {
            self.store
                .delete(&key)
                .await
                .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Keys to delete: everything before the newest `retain`. `keys` must be
/// sorted oldest-first. Pure so the retention rule is unit-testable without
/// touching a store.
pub fn prune_plan(
    keys: &[object_store::path::Path],
    retain: usize,
) -> Vec<object_store::path::Path> {
    if keys.len() <= retain {
        return Vec::new();
    }
    keys[..keys.len() - retain].to_vec()
}

/// One backup export: read the leader's desired state, seal it, upload, prune.
/// Called from the loop with the wall clock; returns an error string per
/// failure so the loop can log without aborting.
pub async fn backup_tick(
    council: &CouncilNode,
    master_key: &[u8; 32],
    store: &BackupStore,
    config: &BackupConfig,
    now: SystemTime,
) -> Result<(), BackupError> {
    let desired = council.desired_state().await;
    let payload =
        serde_json::to_vec(&desired).map_err(|e| BackupError::Serialise(e.to_string()))?;
    let sealed = seal_snapshot(master_key, &payload, config)?;
    store.put(&sealed, now).await?;
    store.prune(config.retain).await?;
    Ok(())
}

/// The long-lived backup loop. Spawned by the binary when
/// `[cluster.backup] url` is set. Only the leader actually exports; followers
/// tick and skip, so it's safe to run on every council node.
pub async fn run_backup_loop(
    council: std::sync::Arc<CouncilNode>,
    master_key: [u8; 32],
    config: BackupConfig,
    shutdown: CancellationToken,
) {
    let Some(url) = config.url.clone() else {
        return;
    };
    let store = match BackupStore::from_url(&url) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("council: backup disabled: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs.max(1)));
    ticker.tick().await; // skip the immediate first tick
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if !council.is_leader().await {
            continue;
        }
        if let Err(e) = backup_tick(&council, &master_key, &store, &config, SystemTime::now()).await
        {
            eprintln!("council: backup export failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::council::types::DesiredState;

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn seal_then_unseal_round_trips_identical_bytes() {
        let master = key(1);
        let payload = b"the desired state payload";
        let sealed = seal_snapshot(&master, payload, &BackupConfig::default()).unwrap();
        let restored = unseal_snapshot(&master, &sealed).unwrap();
        assert_eq!(restored, payload);
    }

    #[test]
    fn round_trips_a_real_desired_state() {
        let master = key(2);
        let mut desired = DesiredState::default();
        desired.config.insert("k".to_string(), "v".to_string());
        let payload = serde_json::to_vec(&desired).unwrap();

        let sealed = seal_snapshot(&master, &payload, &BackupConfig::default()).unwrap();
        let restored_bytes = unseal_snapshot(&master, &sealed).unwrap();
        let restored: DesiredState = serde_json::from_slice(&restored_bytes).unwrap();
        assert_eq!(restored.config.get("k").map(String::as_str), Some("v"));
    }

    #[test]
    fn tampered_ciphertext_fails_to_unseal() {
        let master = key(3);
        let mut sealed = seal_snapshot(&master, b"secret state", &BackupConfig::default()).unwrap();
        // Flip a bit: the GCM tag must reject it.
        sealed.ciphertext[0] ^= 0x01;
        let err = unseal_snapshot(&master, &sealed).unwrap_err();
        assert!(matches!(err, BackupError::Crypto(_)), "got {err:?}");
    }

    #[test]
    fn wrong_master_key_fails_to_unseal() {
        let sealed = seal_snapshot(&key(4), b"state", &BackupConfig::default()).unwrap();
        let err = unseal_snapshot(&key(5), &sealed).unwrap_err();
        assert!(matches!(err, BackupError::Crypto(_)), "got {err:?}");
    }

    #[test]
    fn unknown_envelope_version_is_rejected() {
        let master = key(6);
        let mut sealed = seal_snapshot(&master, b"state", &BackupConfig::default()).unwrap();
        sealed.version = 99;
        let err = unseal_snapshot(&master, &sealed).unwrap_err();
        assert!(
            matches!(err, BackupError::UnsupportedVersion { found: 99, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn encode_decode_round_trips() {
        let sealed = seal_snapshot(&key(7), b"payload", &BackupConfig::default()).unwrap();
        let bytes = encode_backup(&sealed).unwrap();
        let decoded = decode_backup(&bytes).unwrap();
        assert_eq!(decoded, sealed);
    }

    fn path(name: &str) -> object_store::path::Path {
        object_store::path::Path::from(name)
    }

    #[test]
    fn prune_plan_keeps_newest_retain() {
        let keys = vec![
            path("council-001.backup"),
            path("council-002.backup"),
            path("council-003.backup"),
            path("council-004.backup"),
        ];
        let doomed = prune_plan(&keys, 2);
        assert_eq!(
            doomed,
            vec![path("council-001.backup"), path("council-002.backup")]
        );
    }

    #[test]
    fn prune_plan_keeps_all_under_retain() {
        let keys = vec![path("council-001.backup"), path("council-002.backup")];
        assert!(prune_plan(&keys, 7).is_empty());
    }

    #[test]
    fn backup_disabled_by_default() {
        assert!(BackupConfig::default().url.is_none());
    }

    #[tokio::test]
    async fn store_uploads_lists_and_prunes() {
        let dest = tempfile::tempdir().unwrap();
        let store = BackupStore::from_url(&format!("file://{}", dest.path().display())).unwrap();
        let master = key(9);

        // Three backups at distinct timestamps.
        for secs in [1000u64, 2000, 3000] {
            let sealed = seal_snapshot(&master, b"state", &BackupConfig::default()).unwrap();
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            store.put(&sealed, now).await.unwrap();
        }

        let keys = store.list_keys().await.unwrap();
        assert_eq!(keys.len(), 3);

        // Retain 1: the two oldest are pruned; the newest restores.
        let removed = store.prune(1).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.list_keys().await.unwrap().len(), 1);

        let latest = store.latest().await.unwrap().expect("a backup survives");
        let restored = unseal_snapshot(&master, &latest).unwrap();
        assert_eq!(restored, b"state");
    }

    #[tokio::test]
    async fn latest_is_none_on_empty_store() {
        let dest = tempfile::tempdir().unwrap();
        let store = BackupStore::from_url(&format!("file://{}", dest.path().display())).unwrap();
        assert!(store.latest().await.unwrap().is_none());
    }
}

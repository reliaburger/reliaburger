//! Encrypted external council backup (Phase 12b.2, findings D21/CP12).
//!
//! The whitepaper promises the cluster's desired state is reconstructable.
//! Self-healing (H2/D2) keeps the council alive while a *majority* survives,
//! but if every voter dies at once there is nothing left to elect from. The
//! answer is a leader-only periodic export of the state-machine snapshot to
//! external storage, sealed so the blob is useless without the cluster
//! master key.
//!
//! The payload is the state machine's JSON `DesiredState`, the same bytes
//! the #83 snapshot envelope carries. We seal it with a key derived from the cluster master key via HKDF, using a
//! backup-specific info string so the wrapping key can't be confused with any
//! other derived key (CA wrapping, secrets). Every node holds the master key
//! under `/etc/reliaburger`, so any survivor can restore.
//!
//! Upload rides `object_store`, exactly like `bun::snapshot_worker`: one
//! interface over `file://`, `s3://` and `gs://`, credentials from each
//! backend's standard environment variables (see [`crate::object_storage`]).
//!
//! Each object is named by its [`BackupKey`]: recovery epoch, term and log
//! index of the state inside, so restore and retention follow the council's
//! history rather than whichever leader's clock ran fastest.

use std::time::{Duration, SystemTime};

use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::council::node::CouncilNode;
use crate::council::types::{DesiredState, RecoveryId};
use crate::sesame::crypto::{self, CryptoError};

/// HKDF info string that scopes the backup-sealing key. Binding the derived
/// key to this purpose prevents a sealed backup from being decrypted with, or
/// mistaken for, any other key derived from the same master secret.
const BACKUP_SEAL_INFO: &str = "reliaburger-council-backup-seal-v2";

/// Format version of the sealed-backup envelope. Bump when the on-disk shape
/// changes so an older binary refuses a newer blob rather than mis-parsing it.
const BACKUP_ENVELOPE_VERSION: u32 = crate::compatibility::CURRENT.state;

/// Configuration for the council backup loop (`[cluster.backup]`).
///
/// Off by default: `url` unset means the loop never starts, so a cluster that
/// doesn't want external backups pays nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    #[error("not a confirmed leader, skipping the export: {0}")]
    NotLeader(String),
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
    if sealed.hkdf_info != BACKUP_SEAL_INFO {
        return Err(BackupError::Serialise(
            "incompatible backup key context".into(),
        ));
    }
    let wrapping_key = crypto::hkdf_derive_key(master_key, &sealed.hkdf_salt, BACKUP_SEAL_INFO)?;
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

/// Every backup object's name starts with this.
const BACKUP_PREFIX: &str = "council-";
/// Every backup object's name ends with this.
const BACKUP_SUFFIX: &str = ".backup";

/// Where a backup sits in the council's history, and so its object name.
///
/// Backups order by recovery epoch, then Raft term, then the last applied
/// log index: the position of the state they hold, which every leader
/// agrees on. The leader's wall clock only breaks ties. Ordering by the
/// clock alone let a former leader whose clock ran fast keep the "newest"
/// backups, so restores picked its stale state and pruning deleted every
/// correct backup straight after upload (#695).
///
/// The recovery id is in the name too, after everything that orders it. Two
/// recoveries run independently from the same backup land on the same epoch
/// with fresh Raft logs, so their councils can produce backups with the same
/// epoch, term, index and even millisecond. Without the id those would share
/// one object name and the second upload would silently replace the first;
/// with it, both survive and the listing shows which council wrote each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupKey {
    /// The council's recovery epoch when the backup was taken.
    pub recovery_epoch: u64,
    /// The term of the last log entry the backed-up state applied.
    pub term: u64,
    /// The index of the last log entry the backed-up state applied.
    pub index: u64,
    /// Milliseconds since the Unix epoch on the exporting leader's clock.
    pub millis: u64,
    /// The recovery that started `recovery_epoch` (`None` at epoch 0).
    pub recovery_id: Option<RecoveryId>,
}

impl BackupKey {
    /// The key for a backup of `state`, exported at `now`.
    pub fn for_state(state: &DesiredState, now: SystemTime) -> Self {
        let millis = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let applied = state.last_applied_log;
        Self {
            recovery_epoch: state.recovery_epoch,
            term: applied.map_or(0, |log_id| log_id.leader_id.get_term()),
            index: applied.map_or(0, |log_id| log_id.index),
            millis: u64::try_from(millis).unwrap_or(u64::MAX),
            recovery_id: state.recovery_id,
        }
    }

    /// The object name, e.g.
    /// `council-e00000000000000000001-t…-i…-m…-r….backup`. Every number is
    /// zero-padded to its full width, so names sort in key order.
    pub fn object_name(&self) -> String {
        format!(
            "{BACKUP_PREFIX}e{:020}-t{:020}-i{:020}-m{:020}-r{:032x}{BACKUP_SUFFIX}",
            self.recovery_epoch,
            self.term,
            self.index,
            self.millis,
            self.recovery_id.map_or(0, |id| id.0),
        )
    }

    /// Parse an object name written by [`Self::object_name`]. `None` for
    /// anything else, including an older release's `council-{millis}.backup`.
    pub fn parse(name: &str) -> Option<Self> {
        let body = name
            .strip_prefix(BACKUP_PREFIX)?
            .strip_suffix(BACKUP_SUFFIX)?;
        let fields: Vec<&str> = body.split('-').collect();
        let [epoch, term, index, millis, id] = fields.as_slice() else {
            return None;
        };
        let number = |field: &str, tag: char| -> Option<u64> {
            let digits = field.strip_prefix(tag)?;
            (digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| digits.parse().ok())?
        };
        let id = id.strip_prefix('r')?;
        if id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let id = u128::from_str_radix(id, 16).ok()?;
        Some(Self {
            recovery_epoch: number(epoch, 'e')?,
            term: number(term, 't')?,
            index: number(index, 'i')?,
            millis: number(millis, 'm')?,
            recovery_id: (id != 0).then_some(RecoveryId(id)),
        })
    }

    fn order(&self) -> (u64, u64, u64, u64, u128) {
        (
            self.recovery_epoch,
            self.term,
            self.index,
            self.millis,
            self.recovery_id.map_or(0, |id| id.0),
        )
    }
}

impl Ord for BackupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order().cmp(&other.order())
    }
}

impl PartialOrd for BackupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.object_name())
    }
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
            crate::object_storage::open(&parsed).map_err(|e| BackupError::InvalidUrl {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self { store, prefix })
    }

    /// Upload one sealed backup under `key`.
    pub async fn put(&self, key: &BackupKey, sealed: &SealedBackup) -> Result<(), BackupError> {
        let bytes = encode_backup(sealed)?;
        self.store
            .put(&self.location(key), object_store::PutPayload::from(bytes))
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        Ok(())
    }

    fn location(&self, key: &BackupKey) -> object_store::path::Path {
        self.prefix.clone().join(key.object_name())
    }

    /// Every stored backup, oldest first by [`BackupKey`] order: recovery
    /// epoch, then term, then log index, with the wall clock only breaking
    /// ties. An object whose name isn't a backup key (an older binary's
    /// `council-{millis}.backup`, say) is left alone and logged once per
    /// listing, never restored or pruned.
    pub async fn list(&self) -> Result<Vec<BackupKey>, BackupError> {
        use futures_util::StreamExt;
        let mut keys = Vec::new();
        let mut stream = self.store.list(Some(&self.prefix));
        while let Some(entry) = stream.next().await {
            let meta = entry.map_err(|e| BackupError::ObjectStore(e.to_string()))?;
            let Some(name) = meta.location.filename() else {
                continue;
            };
            if !name.ends_with(BACKUP_SUFFIX) {
                continue;
            }
            match BackupKey::parse(name) {
                Some(key) => keys.push(key),
                None => eprintln!(
                    "council: ignoring backup object {} whose name is not an epoch-ordered \
                     backup key (written by an older release?)",
                    meta.location
                ),
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// Fetch and decode the backup stored under `key`.
    pub async fn fetch(&self, key: &BackupKey) -> Result<SealedBackup, BackupError> {
        let result = self
            .store
            .get(&self.location(key))
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
        decode_backup(&bytes)
    }

    /// Fetch and decode the newest stored backup, `None` if the destination
    /// holds none.
    pub async fn latest(&self) -> Result<Option<SealedBackup>, BackupError> {
        let keys = self.list().await?;
        let Some(newest) = keys.last() else {
            return Ok(None);
        };
        Ok(Some(self.fetch(newest).await?))
    }

    /// Prune every backup beyond the newest `retain`, by [`BackupKey`]
    /// order. Returns how many were removed.
    pub async fn prune(&self, retain: usize) -> Result<usize, BackupError> {
        let keys = self.list().await?;
        let doomed = prune_plan(&keys, retain);
        let mut removed = 0;
        for key in doomed {
            self.store
                .delete(&self.location(&key))
                .await
                .map_err(|e| BackupError::ObjectStore(e.to_string()))?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Keys to delete: everything but the newest `retain` by [`BackupKey`]
/// order, oldest first. Pure so the retention rule is unit-testable without
/// touching a store.
pub fn prune_plan(keys: &[BackupKey], retain: usize) -> Vec<BackupKey> {
    let mut sorted = keys.to_vec();
    sorted.sort();
    let doomed = sorted.len().saturating_sub(retain);
    sorted.truncate(doomed);
    sorted
}

/// One backup export: read the leader's desired state, seal it, upload under
/// the key of that state's position, prune. Called from the loop with the
/// wall clock; returns an error per failure so the loop can log without
/// aborting.
///
/// The read is linearizable: a leader that has been deposed and doesn't know
/// it yet fails here instead of exporting stale state. The key's term and
/// index come from the state that read returned, so they always describe
/// the bytes in the backup.
pub async fn backup_tick(
    council: &CouncilNode,
    master_key: &[u8; 32],
    store: &BackupStore,
    config: &BackupConfig,
    now: SystemTime,
) -> Result<(), BackupError> {
    let desired = council
        .desired_state_linearizable()
        .await
        .map_err(|e| BackupError::NotLeader(e.to_string()))?;
    let key = BackupKey::for_state(&desired, now);
    let payload =
        serde_json::to_vec(&desired).map_err(|e| BackupError::Serialise(e.to_string()))?;
    let sealed = seal_snapshot(master_key, &payload, config)?;
    store.put(&key, &sealed).await?;
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
    fn development_backup_and_relabelled_key_context_are_refused() {
        let mut sealed = seal_snapshot(&key(1), b"state", &BackupConfig::default()).unwrap();
        sealed.version = 1;
        assert!(matches!(
            unseal_snapshot(&key(1), &sealed),
            Err(BackupError::UnsupportedVersion { found: 1, .. })
        ));
        sealed.version = BACKUP_ENVELOPE_VERSION;
        sealed.hkdf_info = "reliaburger-council-backup-seal-v1".into();
        assert!(unseal_snapshot(&key(1), &sealed).is_err());
    }

    #[test]
    fn encode_decode_round_trips() {
        let sealed = seal_snapshot(&key(7), b"payload", &BackupConfig::default()).unwrap();
        let bytes = encode_backup(&sealed).unwrap();
        let decoded = decode_backup(&bytes).unwrap();
        assert_eq!(decoded, sealed);
    }

    fn backup_key(recovery_epoch: u64, term: u64, index: u64, millis: u64) -> BackupKey {
        BackupKey {
            recovery_epoch,
            term,
            index,
            millis,
            recovery_id: None,
        }
    }

    fn file_store(dest: &tempfile::TempDir) -> BackupStore {
        BackupStore::from_url(&format!("file://{}", dest.path().display())).unwrap()
    }

    fn applied_at(term: u64, index: u64) -> Option<openraft::LogId<u64>> {
        Some(openraft::LogId::new(
            openraft::CommittedLeaderId::new(term, 1),
            index,
        ))
    }

    #[test]
    fn backup_keys_order_by_epoch_then_term_then_index() {
        let mut keys = vec![
            backup_key(1, 1, 1, 0),
            backup_key(0, 9, 99, 9_000),
            backup_key(0, 9, 100, 1),
            backup_key(0, 10, 1, 1),
            backup_key(2, 0, 0, 0),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                backup_key(0, 9, 99, 9_000),
                backup_key(0, 9, 100, 1),
                backup_key(0, 10, 1, 1),
                backup_key(1, 1, 1, 0),
                backup_key(2, 0, 0, 0),
            ]
        );
        // The object names sort the same way, so a plain listing agrees.
        let ordered: Vec<String> = keys.iter().map(BackupKey::object_name).collect();
        let mut names = ordered.clone();
        names.sort();
        assert_eq!(names, ordered);
    }

    #[test]
    fn a_fast_clock_does_not_win_against_a_higher_term() {
        // A former leader whose clock ran a year fast, and the current
        // leader one term later with a correct clock.
        let year_ms = 365 * 24 * 3600 * 1000;
        let stale = backup_key(0, 4, 500, 1_790_000_000_000 + year_ms);
        let current = backup_key(0, 5, 501, 1_790_000_000_000);
        assert!(current > stale);
        assert_eq!(prune_plan(&[current, stale.clone()], 1), vec![stale]);
    }

    #[test]
    fn prune_keeps_the_newest_by_epoch_and_term() {
        let keys = vec![
            backup_key(1, 1, 10, 100),
            backup_key(0, 7, 900, 999_999),
            backup_key(1, 2, 3, 50),
            backup_key(0, 8, 1, 999_998),
        ];
        assert_eq!(
            prune_plan(&keys, 2),
            vec![backup_key(0, 7, 900, 999_999), backup_key(0, 8, 1, 999_998)]
        );
        assert!(prune_plan(&keys, 7).is_empty());
    }

    #[test]
    fn a_key_round_trips_through_its_object_name() {
        let key = BackupKey {
            recovery_epoch: 3,
            term: u64::MAX,
            index: 42,
            millis: 1_790_000_000_123,
            recovery_id: Some(RecoveryId(0xdead_beef)),
        };
        assert_eq!(BackupKey::parse(&key.object_name()), Some(key));
        let epoch_zero = backup_key(0, 1, 2, 3);
        assert_eq!(
            BackupKey::parse(&epoch_zero.object_name()),
            Some(epoch_zero)
        );
    }

    #[test]
    fn malformed_names_are_not_backup_keys() {
        let good = backup_key(1, 2, 3, 4).object_name();
        for name in [
            "council-0001790000000000.backup".to_string(),
            good.replace(".backup", ".tmp"),
            good.replace("-t", "-x"),
            good.replace("e0000", "e000x"),
            good.replace("-r0", "-rg"),
            format!("{good}.backup"),
        ] {
            assert_eq!(BackupKey::parse(&name), None, "{name}");
        }
    }

    #[test]
    fn the_key_comes_from_the_state_being_backed_up() {
        let state = DesiredState {
            recovery_epoch: 2,
            recovery_id: Some(RecoveryId(7)),
            last_applied_log: applied_at(6, 1234),
            ..Default::default()
        };
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(55);
        assert_eq!(
            BackupKey::for_state(&state, now),
            BackupKey {
                recovery_epoch: 2,
                term: 6,
                index: 1234,
                millis: 55,
                recovery_id: Some(RecoveryId(7)),
            }
        );
    }

    #[tokio::test]
    async fn old_format_keys_are_ignored() {
        let dest = tempfile::tempdir().unwrap();
        let store = file_store(&dest);
        let master = key(8);
        let sealed = seal_snapshot(&master, b"old", &BackupConfig::default()).unwrap();
        let old = dest.path().join("council-9999999999999999.backup");
        std::fs::write(&old, encode_backup(&sealed).unwrap()).unwrap();
        let current = backup_key(0, 1, 1, 1);
        let sealed = seal_snapshot(&master, b"new", &BackupConfig::default()).unwrap();
        store.put(&current, &sealed).await.unwrap();

        assert_eq!(store.list().await.unwrap(), vec![current]);
        let latest = store.latest().await.unwrap().unwrap();
        assert_eq!(unseal_snapshot(&master, &latest).unwrap(), b"new");
        // Retention never touches what it can't order.
        assert_eq!(store.prune(1).await.unwrap(), 0);
        assert!(old.exists());
    }

    /// Two recoveries run independently from one backup land on the same
    /// epoch with fresh logs. Their first backups can agree on epoch, term,
    /// index and millisecond; the recovery id keeps them apart, so neither
    /// overwrites the other at a shared destination.
    #[tokio::test]
    async fn two_recoveries_on_one_epoch_never_share_a_backup_object() {
        let dest = tempfile::tempdir().unwrap();
        let store = file_store(&dest);
        let master = key(10);
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_000_000_000);
        let mut names = Vec::new();
        for (byte, payload) in [(1u8, b"first".as_slice()), (2, b"second")] {
            let mut state = DesiredState::default();
            state.enter_recovery_epoch(0);
            state.recovery_id = Some(RecoveryId(u128::from(byte)));
            state.last_applied_log = applied_at(1, 3);
            let key = BackupKey::for_state(&state, now);
            names.push(key.object_name());
            let sealed = seal_snapshot(&master, payload, &BackupConfig::default()).unwrap();
            store.put(&key, &sealed).await.unwrap();
        }
        assert_ne!(names[0], names[1]);
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        let mut payloads = Vec::new();
        for key in &listed {
            payloads.push(unseal_snapshot(&master, &store.fetch(key).await.unwrap()).unwrap());
        }
        payloads.sort();
        assert_eq!(payloads, vec![b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn backup_disabled_by_default() {
        assert!(BackupConfig::default().url.is_none());
    }

    #[tokio::test]
    async fn store_uploads_lists_and_prunes() {
        let dest = tempfile::tempdir().unwrap();
        let store = file_store(&dest);
        let master = key(9);

        // Three backups at rising log positions.
        for index in [1000u64, 2000, 3000] {
            let payload = index.to_be_bytes();
            let sealed = seal_snapshot(&master, &payload, &BackupConfig::default()).unwrap();
            store
                .put(&backup_key(0, 1, index, 0), &sealed)
                .await
                .unwrap();
        }
        assert_eq!(store.list().await.unwrap().len(), 3);

        // Retain 1: the two oldest are pruned; the newest restores.
        assert_eq!(store.prune(1).await.unwrap(), 2);
        assert_eq!(store.list().await.unwrap(), vec![backup_key(0, 1, 3000, 0)]);
        let latest = store.latest().await.unwrap().expect("a backup survives");
        let restored = unseal_snapshot(&master, &latest).unwrap();
        assert_eq!(restored, 3000u64.to_be_bytes());
    }

    #[tokio::test]
    async fn latest_is_none_on_empty_store() {
        let dest = tempfile::tempdir().unwrap();
        assert!(file_store(&dest).latest().await.unwrap().is_none());
    }

    /// `make test-s3`: a council backup goes up to a real bucket, lists,
    /// prunes and restores by key, with credentials and region taken from
    /// the standard `AWS_*` variables.
    #[tokio::test]
    #[ignore = "requires AWS credentials and RELIABURGER_TEST_S3_URL; run with make test-s3"]
    async fn council_backup_round_trips_through_real_s3() {
        let Ok(base) = std::env::var("RELIABURGER_TEST_S3_URL") else {
            panic!("set RELIABURGER_TEST_S3_URL=s3://bucket/prefix to run this suite");
        };
        let run = format!(
            "{}/council-backup-{}",
            base.trim_end_matches('/'),
            RecoveryId::random()
        );
        let store = BackupStore::from_url(&run).unwrap();
        let master = key(11);
        let older = backup_key(0, 1, 10, u64::MAX);
        let newer = backup_key(0, 2, 11, 1);
        for (key, payload) in [(&older, b"older".as_slice()), (&newer, b"newer")] {
            let sealed = seal_snapshot(&master, payload, &BackupConfig::default()).unwrap();
            store.put(key, &sealed).await.unwrap();
        }
        assert_eq!(store.list().await.unwrap(), vec![older, newer.clone()]);
        assert_eq!(store.prune(1).await.unwrap(), 1);
        let latest = store.latest().await.unwrap().unwrap();
        assert_eq!(unseal_snapshot(&master, &latest).unwrap(), b"newer");
        store.store.delete(&store.location(&newer)).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    /// Pruning deletes older backups on the strength of the new one, so the
    /// new one must be durable first.
    #[test]
    fn local_backup_destination_syncs_before_pruning() {
        let dest = tempfile::tempdir().unwrap();
        let store = BackupStore::from_url(&format!("file://{}", dest.path().display())).unwrap();
        let store = format!("{:?}", store.store);
        assert!(store.contains("fsync: true"), "{store}");
    }
}

//! Node-level upgrade manager: fetch, verify, stage, swap, exec, revert.
//!
//! The dangerous parts are split in two so everything up to the point of no
//! return is unit-testable:
//!
//! - [`UpgradeManager::prepare`] — fetch the binary, verify signatures,
//!   stage it in the store, write the `Staged` marker. Fails cleanly; the
//!   running system is untouched (the symlink has not moved).
//! - [`UpgradeManager::execute`] — write the `Executed` marker, swap the
//!   symlink, `execv` the new binary. Only returns on error, in which case
//!   it puts the symlink back.
//!
//! Startup-side recovery ([`UpgradeManager::startup_action`]) wraps the pure
//! [`decide_startup`] state machine with the filesystem actions it demands.

use std::path::{Path, PathBuf};

use super::error::UpgradeError;
use super::marker::{
    InstanceInventory, MarkerPhase, StartupDecision, UpgradeMarker, decide_startup,
};
use super::signing::{self, PublicKey, SignatureEnvelope};
use super::store::BinaryStore;
use super::types::{
    BinarySource, NodeUpgradeStatus, UpgradeDirective, UpgradeHistoryEntry, UpgradeOutcome,
};
use super::version::BinaryVersion;

/// How many history entries `status()` returns.
const STATUS_HISTORY_LIMIT: usize = 20;

/// A prepared upgrade: verified, staged, marked. Ready for [`execute`].
///
/// [`execute`]: UpgradeManager::execute
#[derive(Debug)]
pub struct PreparedUpgrade {
    marker: UpgradeMarker,
}

impl PreparedUpgrade {
    /// The version this prepared upgrade swaps to.
    pub fn target_version(&self) -> &BinaryVersion {
        &self.marker.target_version
    }
}

/// What a bun startup must do about upgrade state, with filesystem effects
/// already applied. Returned by [`UpgradeManager::startup_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupAction {
    /// Boot normally. If `verify` is set, this process is a freshly
    /// swapped-in version and must run post-boot verification, then call
    /// [`UpgradeManager::commit`] or [`UpgradeManager::mark_revert_pending`].
    Continue { verify: Option<UpgradeMarker> },
    /// The symlink has been reverted; exec the previous binary now via
    /// [`UpgradeManager::exec_current_symlink`]. (Returned instead of
    /// exec'ing directly so the caller controls final flushes.)
    ExecPrevious,
}

/// Node-level upgrade manager. One per bun process.
#[derive(Debug, Clone)]
pub struct UpgradeManager {
    store: BinaryStore,
    marker_path: PathBuf,
    history_path: PathBuf,
    running_version: BinaryVersion,
    /// argv captured at startup; passed to the exec'd binary (argv[0] is
    /// replaced with the symlink path).
    original_argv: Vec<String>,
    release_keys: Vec<PublicKey>,
    external_key: Option<PublicKey>,
    retain_versions: u32,
    max_boot_attempts: u32,
    /// How this node addresses peers, so a binary fetch from Pickle uses the
    /// scheme the registry actually serves and a client that trusts the
    /// cluster CA (O3). Defaults to plaintext; `bun` sets the real one.
    ///
    /// Integrity was never at stake here — the sha256 gate and the embedded
    /// release signature are checked on every path regardless. What plaintext
    /// cost was *working at all* against a TLS-only registry, plus disclosing
    /// which build a node is moving to.
    cluster_http: crate::cluster::ClusterHttp,
}

/// Derive the store stem from the executable path bun was invoked as.
///
/// If invoked via the entry symlink (`/usr/local/bin/bun`), the symlink's
/// file name is the stem. If invoked directly as a versioned file
/// (`bun-v0.1.0`), strip the `-vX.Y.Z` suffix. A plain un-versioned binary
/// (first install, `target/debug/bun`) is its own stem.
pub fn derive_stem(invoked_path: &Path) -> String {
    let name = invoked_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bun");
    if let Some((stem, suffix)) = name.rsplit_once("-v")
        && !stem.is_empty()
        && format!("v{suffix}").parse::<BinaryVersion>().is_ok()
    {
        return stem.to_string();
    }
    name.to_string()
}

impl UpgradeManager {
    /// Build a manager from node config and the path bun was invoked as
    /// (`std::env::current_exe()`, un-canonicalised, so the symlink name is
    /// still visible).
    pub fn new(
        config: &crate::config::node::UpgradeSection,
        data_dir: &Path,
        invoked_path: &Path,
        running_version: BinaryVersion,
        original_argv: Vec<String>,
    ) -> Result<Self, UpgradeError> {
        let binary_dir = match &config.binary_dir {
            Some(dir) => dir.clone(),
            None => {
                let resolved = std::fs::canonicalize(invoked_path)?;
                resolved.parent().map(Path::to_path_buf).ok_or_else(|| {
                    UpgradeError::InvalidMarker {
                        path: resolved.clone(),
                        reason: "executable has no parent directory".to_string(),
                    }
                })?
            }
        };
        let stem = derive_stem(invoked_path);
        let release_keys = super::keys::release_keys(config)?;
        let external_key = config
            .external_signing_key
            .as_deref()
            .map(signing::parse_public_key)
            .transpose()?;

        Ok(Self {
            store: BinaryStore::new(binary_dir, stem),
            marker_path: UpgradeMarker::path_in(data_dir),
            history_path: data_dir.join("upgrade").join("history.jsonl"),
            running_version,
            original_argv,
            release_keys,
            external_key,
            retain_versions: config.retain_versions,
            max_boot_attempts: config.max_boot_attempts,
            cluster_http: crate::cluster::ClusterHttp::plaintext(),
        })
    }

    /// Address peers the way this node's cluster plane does (O3).
    ///
    /// Builder-style rather than a `new` parameter: every test constructs a
    /// manager and none of them need TLS, so only `bun` has to say so.
    pub fn with_cluster_http(mut self, cluster_http: crate::cluster::ClusterHttp) -> Self {
        self.cluster_http = cluster_http;
        self
    }

    /// Attach the internal service token as the bearer for binary fetches (B2).
    ///
    /// A Pickle binary fetch on a routable cluster requires a principal
    /// (`require_read_auth`); without the token every self-upgrade download
    /// 401s. Only `bun` has the token, so it sets this after deriving it.
    pub fn with_bearer(mut self, bearer: Option<String>) -> Self {
        self.cluster_http = self.cluster_http.with_bearer(bearer);
        self
    }

    /// The version this process is running.
    pub fn running_version(&self) -> &BinaryVersion {
        &self.running_version
    }

    /// Is an upgrade currently in flight on this node? (Cheap: one stat.)
    pub fn upgrade_in_flight(&self) -> bool {
        self.marker_path.exists()
    }

    /// The binary store (leader-side orchestration stages into it too).
    pub fn store(&self) -> &BinaryStore {
        &self.store
    }

    /// Upgrade ids this node attempted and reverted (recent history).
    /// The leader polls these to detect node-side reverts.
    pub fn reverted_upgrade_ids(&self) -> Vec<String> {
        self.read_history()
            .into_iter()
            .filter(|entry| entry.outcome == UpgradeOutcome::Reverted)
            .map(|entry| entry.upgrade_id)
            .collect()
    }

    /// Node-level status: running version, in-flight marker, recent history.
    pub fn status(&self) -> NodeUpgradeStatus {
        let in_flight = UpgradeMarker::load(&self.marker_path).ok().flatten();
        let history = self.read_history();
        NodeUpgradeStatus {
            running_version: self.running_version.clone(),
            in_flight,
            history,
        }
    }

    // -----------------------------------------------------------------
    // Upgrade path: prepare -> execute
    // -----------------------------------------------------------------

    /// Fetch, verify, and stage the directive's binary; write the `Staged`
    /// marker with the pre-upgrade workload inventory.
    ///
    /// Returns `Ok(None)` if the same `upgrade_id` is already in flight
    /// (idempotent re-delivery). Fails without touching the running system
    /// otherwise — the symlink only moves in [`execute`](Self::execute).
    pub async fn prepare(
        &self,
        directive: &UpgradeDirective,
        pre_upgrade_instances: Vec<InstanceInventory>,
    ) -> Result<Option<PreparedUpgrade>, UpgradeError> {
        if let Some(existing) = UpgradeMarker::load(&self.marker_path)? {
            if existing.upgrade_id == directive.upgrade_id {
                return Ok(None);
            }
            return Err(UpgradeError::AlreadyInFlight {
                upgrade_id: existing.upgrade_id,
            });
        }
        // Never re-attempt an id this node already reverted: after a revert
        // the marker is gone, so without this check a re-delivered (or
        // leader-retried) directive would crash-loop the node forever.
        // Retries get a fresh id (orchestrator::resume renames the run).
        if self.reverted_upgrade_ids().contains(&directive.upgrade_id) {
            return Err(UpgradeError::PreviouslyFailed {
                upgrade_id: directive.upgrade_id.clone(),
            });
        }

        let bytes = self.fetch_binary(directive).await?;
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: directive.binary_sha256.clone(),
            embedded: directive.embedded_signature.clone(),
            external: directive.external_signature.clone(),
        };
        // Treat the upgrade as network — and so demand the external signature —
        // when the bytes came from the network by either route (M5): a Pickle
        // fetch, or a single-node download staged as a local file.
        let is_network = directive.source.is_network() || directive.network_provenance;
        signing::verify_binary(
            &bytes,
            &envelope,
            &self.release_keys,
            self.external_key.as_ref(),
            is_network,
        )?;

        // First upgrade from an un-versioned install: adopt the running
        // binary into the store so rollback has something to return to.
        self.adopt_running_binary_if_missing()?;

        self.store
            .stage(&directive.target_version, &bytes, &envelope)?;

        let marker = UpgradeMarker {
            schema: 1,
            upgrade_id: directive.upgrade_id.clone(),
            previous_version: self.running_version.clone(),
            previous_binary: self.running_version.file_name(
                self.store
                    .symlink_path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("bun"),
            ),
            target_version: directive.target_version.clone(),
            target_binary: directive.target_version.file_name(
                self.store
                    .symlink_path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("bun"),
            ),
            phase: MarkerPhase::Staged,
            boot_attempts: 0,
            pre_upgrade_instances,
        };
        marker.store(&self.marker_path)?;

        Ok(Some(PreparedUpgrade { marker }))
    }

    /// The point of no return: record `Executed`, swap the symlink, exec.
    ///
    /// On success this never returns — the process image is replaced. It
    /// returns only on error, after putting the symlink back on the
    /// previous version and archiving the marker.
    pub fn execute(&self, prepared: PreparedUpgrade) -> UpgradeError {
        let mut marker = prepared.marker;
        marker.phase = MarkerPhase::Executed;
        if let Err(e) = marker.store(&self.marker_path) {
            let _ = UpgradeMarker::remove(&self.marker_path);
            return e;
        }
        if let Err(e) = self.store.activate(&marker.target_version) {
            let _ = UpgradeMarker::remove(&self.marker_path);
            return e;
        }

        let error = self.exec_current_symlink();

        // Exec failed (missing binary, ENOEXEC...): put the previous version
        // back. Only archive the marker if that restore succeeded — otherwise
        // the current symlink still points at the broken target, so we must
        // keep the marker so the next boot's check triggers a revert rather
        // than exec the broken binary again with no marker (M17).
        if let Err(restore_err) = self.store.activate(&marker.previous_version) {
            eprintln!(
                "bun: CRITICAL: failed to restore the previous version after a failed exec \
                 ({restore_err}); leaving the upgrade marker in place so the next boot reverts"
            );
        } else {
            let _ = UpgradeMarker::archive_stale(&self.marker_path);
        }
        let _ = self.append_history(&UpgradeHistoryEntry {
            upgrade_id: marker.upgrade_id.clone(),
            from_version: marker.previous_version.clone(),
            to_version: marker.target_version.clone(),
            outcome: UpgradeOutcome::Abandoned,
            detail: format!("exec failed: {error}"),
            recorded_at: std::time::SystemTime::now(),
        });
        error
    }

    /// Replace this process with whatever the entry symlink points at,
    /// passing the original argv. Never returns on success.
    pub fn exec_current_symlink(&self) -> UpgradeError {
        use std::ffi::CString;

        let path = self.store.symlink_path();
        let Ok(path_c) = CString::new(path.to_string_lossy().into_owned()) else {
            return UpgradeError::ExecFailed {
                reason: "binary path contains a NUL byte".to_string(),
            };
        };
        // argv[0] is the symlink path; the rest is the original command line
        // (config path, --cluster, ...), which the new binary re-parses.
        let mut argv_c = vec![path_c.clone()];
        for arg in self.original_argv.iter().skip(1) {
            match CString::new(arg.as_str()) {
                Ok(c) => argv_c.push(c),
                Err(_) => {
                    return UpgradeError::ExecFailed {
                        reason: format!("argument contains a NUL byte: {arg:?}"),
                    };
                }
            }
        }

        // execv only returns on failure.
        let err = nix::unistd::execv(&path_c, &argv_c)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "execv returned without error".to_string());
        UpgradeError::ExecFailed { reason: err }
    }

    // -----------------------------------------------------------------
    // Rollback path
    // -----------------------------------------------------------------

    /// Prepare a rollback to `version` (default: the newest installed
    /// version older than the running one). No download, no signature
    /// re-check — the binary was verified when it was first staged.
    pub fn prepare_rollback(
        &self,
        version: Option<BinaryVersion>,
        pre_upgrade_instances: Vec<InstanceInventory>,
    ) -> Result<PreparedUpgrade, UpgradeError> {
        if let Some(existing) = UpgradeMarker::load(&self.marker_path)? {
            return Err(UpgradeError::AlreadyInFlight {
                upgrade_id: existing.upgrade_id,
            });
        }

        let target = match version {
            Some(version) => version,
            None => {
                let mut installed = self.store.installed_versions()?;
                installed.sort();
                installed
                    .into_iter()
                    .rfind(|v| *v < self.running_version)
                    .ok_or(UpgradeError::NoRollbackTarget)?
            }
        };
        if !self.store.binary_path(&target).is_file() {
            return Err(UpgradeError::UnknownVersion { version: target });
        }

        // Re-verify the stored binary against its signature envelope before
        // staging it for exec (O4). Staging a binary already implied code-exec
        // trust, but re-checking catches on-disk tampering or bit-rot between
        // the original stage and the rollback, for the cost of a hash and a
        // couple of signature verifications. Only a binary that carries a real
        // signature is re-verified: a pre-existing / directly-installed binary
        // has no `.sig`, and one adopted from the running process carries a stub
        // envelope with an empty embedded signature — both are trusted by virtue
        // of already being on disk / executing, so there is nothing to check.
        // An envelope with an external signature is verified as a network
        // artefact (both signatures required); otherwise just the embedded one.
        if let Ok(envelope) = SignatureEnvelope::load(&self.store.envelope_path(&target))
            && !envelope.embedded.is_empty()
        {
            let bytes = std::fs::read(self.store.binary_path(&target))?;
            signing::verify_binary(
                &bytes,
                &envelope,
                &self.release_keys,
                self.external_key.as_ref(),
                envelope.external.is_some(),
            )?;
        }

        let stem = self
            .store
            .symlink_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bun")
            .to_string();
        self.adopt_running_binary_if_missing()?;
        let marker = UpgradeMarker {
            schema: 1,
            upgrade_id: format!("rollback-to-{target}"),
            previous_version: self.running_version.clone(),
            previous_binary: self.running_version.file_name(&stem),
            target_version: target.clone(),
            target_binary: target.file_name(&stem),
            phase: MarkerPhase::Staged,
            boot_attempts: 0,
            pre_upgrade_instances,
        };
        marker.store(&self.marker_path)?;
        Ok(PreparedUpgrade { marker })
    }

    // -----------------------------------------------------------------
    // Startup-side recovery
    // -----------------------------------------------------------------

    /// Run the startup decision and apply its filesystem effects.
    ///
    /// Call once, immediately after config load, before subsystems start.
    pub fn startup_action(&self) -> Result<StartupAction, UpgradeError> {
        let marker = match UpgradeMarker::load(&self.marker_path) {
            Ok(marker) => marker,
            Err(e) => {
                // Corrupt marker: archive it rather than refuse to boot.
                eprintln!("bun: warning: {e}; archiving marker");
                let _ = UpgradeMarker::archive_stale(&self.marker_path);
                None
            }
        };

        match decide_startup(marker, &self.running_version, self.max_boot_attempts) {
            StartupDecision::NormalBoot => Ok(StartupAction::Continue { verify: None }),

            StartupDecision::VerifyUpgrade { marker } => {
                marker.store(&self.marker_path)?;
                Ok(StartupAction::Continue {
                    verify: Some(marker),
                })
            }

            StartupDecision::RevertAndExecPrevious { marker } => {
                eprintln!(
                    "bun: upgrade to {} failed after {} boot attempt(s); reverting to {}",
                    marker.target_version, marker.boot_attempts, marker.previous_version
                );
                marker.store(&self.marker_path)?;
                self.store.activate(&marker.previous_version)?;
                Ok(StartupAction::ExecPrevious)
            }

            StartupDecision::CompleteRevert { marker } => {
                self.append_history(&UpgradeHistoryEntry {
                    upgrade_id: marker.upgrade_id.clone(),
                    from_version: marker.previous_version.clone(),
                    to_version: marker.target_version.clone(),
                    outcome: UpgradeOutcome::Reverted,
                    detail: format!(
                        "reverted after {} boot attempt(s) on {}",
                        marker.boot_attempts, marker.target_version
                    ),
                    recorded_at: std::time::SystemTime::now(),
                })?;
                let _ = UpgradeMarker::remove(&self.marker_path);
                eprintln!(
                    "bun: revert to {} complete; upgrade {} failed",
                    marker.previous_version, marker.upgrade_id
                );
                Ok(StartupAction::Continue { verify: None })
            }

            StartupDecision::ArchiveStaleMarker { reason } => {
                eprintln!("bun: warning: archiving stale upgrade marker: {reason}");
                let _ = UpgradeMarker::archive_stale(&self.marker_path);
                Ok(StartupAction::Continue { verify: None })
            }
        }
    }

    /// Post-boot verification succeeded: the swap is permanent. Records
    /// history, deletes the marker, prunes old binaries.
    pub fn commit(&self, marker: &UpgradeMarker) -> Result<(), UpgradeError> {
        self.append_history(&UpgradeHistoryEntry {
            upgrade_id: marker.upgrade_id.clone(),
            from_version: marker.previous_version.clone(),
            to_version: marker.target_version.clone(),
            outcome: UpgradeOutcome::Committed,
            detail: format!("verified after {} boot attempt(s)", marker.boot_attempts),
            recorded_at: std::time::SystemTime::now(),
        })?;
        // Prune old binaries *before* clearing the in-flight marker.
        // `upgrade_in_flight` is a single stat on the marker file, so a node
        // reports "settled" the instant the marker is gone. If GC ran after
        // that, an observer could catch the store mid-prune — a binary
        // already removed but its `.sig` sidecar not yet — which is exactly
        // the race the retention test hit on a loaded runner. Retention GC is
        // best-effort: failing to prune an old binary must never fail an
        // otherwise-verified upgrade.
        // Protect BOTH the version we rolled back from (`previous_version`,
        // kept for a re-roll-forward) AND the version the symlink now points
        // at (`target_version`, the live binary). A rollback to a version
        // older than the retention window would otherwise leave `target_version`
        // among the deletion candidates and GC would delete the running
        // binary — the symlink then dangles and the next exec/restart hits
        // ENOENT with no automatic revert.
        match self.store.garbage_collect(
            self.retain_versions,
            &[
                marker.previous_version.clone(),
                marker.target_version.clone(),
            ],
        ) {
            Ok(deleted) => {
                for version in deleted {
                    println!("bun: retention gc removed binary {version}");
                }
            }
            Err(e) => eprintln!("bun: warning: retention gc failed: {e}"),
        }
        UpgradeMarker::remove(&self.marker_path)?;
        Ok(())
    }

    /// Post-boot verification failed: flag for revert. The caller should
    /// exit(1) afterwards; the supervisor restarts us and startup reverts.
    pub fn mark_revert_pending(
        &self,
        marker: &UpgradeMarker,
        reason: &str,
    ) -> Result<(), UpgradeError> {
        eprintln!(
            "bun: upgrade verification failed ({reason}); reverting to {}",
            marker.previous_version
        );
        let mut marker = marker.clone();
        marker.phase = MarkerPhase::RevertPending;
        marker.store(&self.marker_path)
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    async fn fetch_binary(&self, directive: &UpgradeDirective) -> Result<Vec<u8>, UpgradeError> {
        match &directive.source {
            BinarySource::LocalFile { path } => Ok(tokio::fs::read(path).await?),
            BinarySource::Pickle { registry_address } => {
                // Pickle stores the binary as a content-addressed blob under
                // a single-segment repository name (axum {name} routes).
                let url = self.cluster_http.url(
                    registry_address,
                    &format!(
                        "/v2/{}/blobs/sha256:{}",
                        super::BINARY_BLOB_REPO,
                        directive.binary_sha256
                    ),
                );
                // `get` carries the internal service token as a bearer: on a
                // routable cluster the registry sets `require_read_auth`, so a
                // bearer-less binary fetch 401s (B2).
                let response = self.cluster_http.get(&url).send().await.map_err(|e| {
                    UpgradeError::FetchFailed {
                        url: url.clone(),
                        reason: e.to_string(),
                    }
                })?;
                if !response.status().is_success() {
                    return Err(UpgradeError::FetchFailed {
                        url,
                        reason: format!("status {}", response.status()),
                    });
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| UpgradeError::FetchFailed {
                        url,
                        reason: e.to_string(),
                    })?;
                Ok(bytes.to_vec())
            }
        }
    }

    /// First-upgrade bootstrap: if the running version has no versioned
    /// file in the store (plain `bun` install), copy the current
    /// executable in so rollback has a target.
    fn adopt_running_binary_if_missing(&self) -> Result<(), UpgradeError> {
        let path = self.store.binary_path(&self.running_version);
        if path.is_file() {
            return Ok(());
        }
        let current = std::fs::canonicalize(std::env::current_exe()?)?;
        let bytes = std::fs::read(&current)?;
        // No signature envelope for a pre-existing binary; store a stub so
        // the file pair stays consistent. It is never re-verified locally.
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: signing::sha256_hex(&bytes),
            embedded: String::new(),
            external: None,
        };
        self.store.stage(&self.running_version, &bytes, &envelope)?;
        Ok(())
    }

    fn append_history(&self, entry: &UpgradeHistoryEntry) -> Result<(), UpgradeError> {
        use std::io::Write as _;
        if let Some(dir) = self.history_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // History is plain data; serialisation cannot fail.
        let line = serde_json::to_string(entry).expect("history entry serialises");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    fn read_history(&self) -> Vec<UpgradeHistoryEntry> {
        let Ok(contents) = std::fs::read_to_string(&self.history_path) else {
            return Vec::new();
        };
        let mut entries: Vec<UpgradeHistoryEntry> = contents
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if entries.len() > STATUS_HISTORY_LIMIT {
            entries.drain(..entries.len() - STATUS_HISTORY_LIMIT);
        }
        entries
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::node::UpgradeSection;
    use crate::upgrade::signing::{encode_public_key, generate_keypair, sha256_hex, sign};

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        manager: UpgradeManager,
        release_pkcs8: Vec<u8>,
        external_pkcs8: Vec<u8>,
        binary_dir: PathBuf,
    }

    /// A manager over a temp binary dir + data dir, with throwaway keys and
    /// a fake "running" binary installed as bun-v0.1.0 (symlinked).
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let binary_dir = dir.path().join("bin");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&binary_dir).unwrap();

        let (release_pkcs8, release_public) = generate_keypair().unwrap();
        let (external_pkcs8, external_public) = generate_keypair().unwrap();

        // Install the "running" binary and the entry symlink.
        let store = BinaryStore::new(binary_dir.clone(), "bun".to_string());
        let stub_envelope = SignatureEnvelope {
            schema: 1,
            sha256: sha256_hex(b"old binary"),
            embedded: String::new(),
            external: None,
        };
        store
            .stage(&v("0.1.0"), b"old binary", &stub_envelope)
            .unwrap();
        store.activate(&v("0.1.0")).unwrap();

        let config = UpgradeSection {
            external_signing_key: Some(encode_public_key(&external_public)),
            binary_dir: Some(binary_dir.clone()),
            release_keys_override: Some(vec![encode_public_key(&release_public)]),
            ..UpgradeSection::default()
        };
        let manager = UpgradeManager::new(
            &config,
            &data_dir,
            &binary_dir.join("bun"),
            v("0.1.0"),
            vec![
                "bun".to_string(),
                "--config".to_string(),
                "x.toml".to_string(),
            ],
        )
        .unwrap();

        Fixture {
            _dir: dir,
            manager,
            release_pkcs8,
            external_pkcs8,
            binary_dir,
        }
    }

    fn directive_for(fixture: &Fixture, bytes: &[u8], id: &str) -> UpgradeDirective {
        let path = fixture.binary_dir.join("incoming-binary");
        std::fs::write(&path, bytes).unwrap();
        UpgradeDirective {
            upgrade_id: id.to_string(),
            target_version: v("0.2.0"),
            binary_sha256: sha256_hex(bytes),
            embedded_signature: sign(&fixture.release_pkcs8, bytes).unwrap(),
            external_signature: Some(sign(&fixture.external_pkcs8, bytes).unwrap()),
            source: BinarySource::LocalFile { path },
            network_provenance: false,
        }
    }

    fn inventory() -> Vec<InstanceInventory> {
        vec![InstanceInventory {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            instance_id: 0,
            pid: 4242,
            full_id: "default__web-0".to_string(),
        }]
    }

    #[tokio::test]
    async fn prepare_stages_binary_and_writes_staged_marker() {
        let fixture = fixture();
        let directive = directive_for(&fixture, b"new binary", "up-1");

        let prepared = fixture
            .manager
            .prepare(&directive, inventory())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(prepared.target_version(), &v("0.2.0"));
        // Staged but NOT activated: the symlink still points at 0.1.0.
        assert!(fixture.binary_dir.join("bun-v0.2.0").is_file());
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );
        let status = fixture.manager.status();
        let marker = status.in_flight.unwrap();
        assert_eq!(marker.phase, MarkerPhase::Staged);
        assert_eq!(marker.pre_upgrade_instances, inventory());
    }

    #[tokio::test]
    async fn apply_rejects_second_concurrent_upgrade() {
        let fixture = fixture();
        let first = directive_for(&fixture, b"new binary", "up-1");
        fixture
            .manager
            .prepare(&first, vec![])
            .await
            .unwrap()
            .unwrap();

        let second = directive_for(&fixture, b"other binary", "up-2");
        let err = fixture.manager.prepare(&second, vec![]).await.unwrap_err();
        assert!(matches!(err, UpgradeError::AlreadyInFlight { .. }));
    }

    #[tokio::test]
    async fn apply_is_idempotent_for_same_upgrade_id() {
        let fixture = fixture();
        let directive = directive_for(&fixture, b"new binary", "up-1");
        fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap()
            .unwrap();

        // Re-delivery of the same directive: no error, nothing to execute.
        let again = fixture.manager.prepare(&directive, vec![]).await.unwrap();
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn apply_verifies_before_staging() {
        let fixture = fixture();
        let mut directive = directive_for(&fixture, b"new binary", "up-1");
        // Corrupt the external signature.
        directive.external_signature =
            Some(sign(&fixture.release_pkcs8, b"different bytes").unwrap());

        let err = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(matches!(err, UpgradeError::ExternalSignatureInvalid));
        // Nothing staged, no marker, symlink untouched.
        assert!(!fixture.binary_dir.join("bun-v0.2.0").exists());
        assert!(!fixture.manager.upgrade_in_flight());
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );
    }

    #[tokio::test]
    async fn successful_verification_commits_and_gcs() {
        let fixture = fixture();
        // Install enough versions that GC has something to do.
        let stub = SignatureEnvelope {
            schema: 1,
            sha256: String::new(),
            embedded: String::new(),
            external: None,
        };
        for version in ["0.0.1", "0.0.2", "0.0.3"] {
            fixture
                .manager
                .store()
                .stage(&v(version), b"x", &stub)
                .unwrap();
        }

        let directive = directive_for(&fixture, b"new binary", "up-1");
        let prepared = fixture
            .manager
            .prepare(&directive, inventory())
            .await
            .unwrap()
            .unwrap();

        fixture.manager.commit(&prepared.marker).unwrap();

        assert!(!fixture.manager.upgrade_in_flight());
        let status = fixture.manager.status();
        assert_eq!(status.history.len(), 1);
        assert_eq!(status.history[0].outcome, UpgradeOutcome::Committed);
        // retain_versions default 3, previous version protected: the very
        // oldest stubs are gone.
        assert!(!fixture.binary_dir.join("bun-v0.0.1").exists());
        assert!(fixture.binary_dir.join("bun-v0.1.0").is_file());
    }

    #[test]
    fn commit_never_gc_deletes_the_current_symlink_target_on_rollback() {
        let fixture = fixture();
        let stub = SignatureEnvelope {
            schema: 1,
            sha256: String::new(),
            embedded: String::new(),
            external: None,
        };
        // Store holds 0.1.0 (active, from the fixture) plus four newer
        // versions: five in total, retention default is 3.
        for version in ["0.2.0", "0.3.0", "0.4.0", "0.5.0"] {
            fixture
                .manager
                .store()
                .stage(&v(version), b"x", &stub)
                .unwrap();
        }
        // The symlink still targets 0.1.0 — the version we rolled back to,
        // which sorts oldest and would be a GC candidate.
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );

        let marker = UpgradeMarker {
            schema: 1,
            upgrade_id: "rollback-to-0.1.0".to_string(),
            previous_version: v("0.5.0"),
            previous_binary: v("0.5.0").file_name("bun"),
            target_version: v("0.1.0"),
            target_binary: v("0.1.0").file_name("bun"),
            phase: MarkerPhase::Executed,
            boot_attempts: 1,
            pre_upgrade_instances: vec![],
        };

        fixture.manager.commit(&marker).unwrap();

        // The live binary and its symlink must survive the retention sweep;
        // only a genuinely superseded, unprotected version (0.2.0) is pruned.
        assert!(
            fixture.binary_dir.join("bun-v0.1.0").is_file(),
            "GC deleted the running rollback target — the node would fail to exec"
        );
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );
        assert!(!fixture.binary_dir.join("bun-v0.2.0").exists());
    }

    #[tokio::test]
    async fn verification_failure_marks_revert_pending() {
        let fixture = fixture();
        let directive = directive_for(&fixture, b"new binary", "up-1");
        let prepared = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap()
            .unwrap();

        fixture
            .manager
            .mark_revert_pending(&prepared.marker, "adopted instances missing")
            .unwrap();

        let status = fixture.manager.status();
        assert_eq!(status.in_flight.unwrap().phase, MarkerPhase::RevertPending);
    }

    #[tokio::test]
    async fn rollback_rejects_version_not_on_disk() {
        let fixture = fixture();
        let err = fixture
            .manager
            .prepare_rollback(Some(v("0.0.9")), vec![])
            .unwrap_err();
        assert!(matches!(err, UpgradeError::UnknownVersion { .. }));
    }

    #[tokio::test]
    async fn rollback_defaults_to_newest_older_version() {
        let dir = tempfile::tempdir().unwrap();
        let binary_dir = dir.path().join("bin");
        std::fs::create_dir_all(&binary_dir).unwrap();
        let store = BinaryStore::new(binary_dir.clone(), "bun".to_string());
        let stub = SignatureEnvelope {
            schema: 1,
            sha256: String::new(),
            embedded: String::new(),
            external: None,
        };
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            store.stage(&v(version), b"x", &stub).unwrap();
        }
        store.activate(&v("0.3.0")).unwrap();

        let config = UpgradeSection {
            binary_dir: Some(binary_dir.clone()),
            ..UpgradeSection::default()
        };
        let manager = UpgradeManager::new(
            &config,
            &dir.path().join("data"),
            &binary_dir.join("bun"),
            v("0.3.0"),
            vec!["bun".to_string()],
        )
        .unwrap();

        let prepared = manager.prepare_rollback(None, vec![]).unwrap();
        assert_eq!(prepared.target_version(), &v("0.2.0"));
    }

    /// O4: a rollback target whose stored bytes no longer match its signature
    /// envelope (on-disk tampering or rot) is refused before it's staged for
    /// exec. A stub-envelope (adopted) binary is exempt — it carries no
    /// signature and is trusted by virtue of having been the running process.
    #[tokio::test]
    async fn rollback_rejects_a_tampered_signed_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary_dir = dir.path().join("bin");
        std::fs::create_dir_all(&binary_dir).unwrap();
        let (release_pkcs8, release_public) = generate_keypair().unwrap();
        let store = BinaryStore::new(binary_dir.clone(), "bun".to_string());

        // A properly-signed older version, and a running version (adopted stub).
        let old = b"old signed binary";
        store
            .stage(
                &v("0.1.0"),
                old,
                &SignatureEnvelope {
                    schema: 1,
                    sha256: sha256_hex(old),
                    embedded: sign(&release_pkcs8, old).unwrap(),
                    external: None,
                },
            )
            .unwrap();
        let running = b"running binary";
        store
            .stage(
                &v("0.2.0"),
                running,
                &SignatureEnvelope {
                    schema: 1,
                    sha256: sha256_hex(running),
                    embedded: String::new(),
                    external: None,
                },
            )
            .unwrap();
        store.activate(&v("0.2.0")).unwrap();

        // Corrupt the signed 0.1.0 bytes on disk after staging.
        std::fs::write(store.binary_path(&v("0.1.0")), b"TAMPERED").unwrap();

        let config = UpgradeSection {
            binary_dir: Some(binary_dir.clone()),
            release_keys_override: Some(vec![encode_public_key(&release_public)]),
            ..UpgradeSection::default()
        };
        let manager = UpgradeManager::new(
            &config,
            &dir.path().join("data"),
            &binary_dir.join("bun"),
            v("0.2.0"),
            vec!["bun".to_string()],
        )
        .unwrap();

        let err = manager.prepare_rollback(None, vec![]).unwrap_err();
        assert!(
            matches!(err, UpgradeError::HashMismatch { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn first_upgrade_adopts_unversioned_binary_into_store() {
        // A store with no versioned files at all (fresh install).
        let dir = tempfile::tempdir().unwrap();
        let binary_dir = dir.path().join("bin");
        std::fs::create_dir_all(&binary_dir).unwrap();

        let (release_pkcs8, release_public) = generate_keypair().unwrap();
        let config = UpgradeSection {
            binary_dir: Some(binary_dir.clone()),
            release_keys_override: Some(vec![encode_public_key(&release_public)]),
            ..UpgradeSection::default()
        };
        let manager = UpgradeManager::new(
            &config,
            &dir.path().join("data"),
            &binary_dir.join("bun"),
            v("0.1.0"),
            vec!["bun".to_string()],
        )
        .unwrap();

        let bytes = b"new binary";
        let path = binary_dir.join("incoming");
        std::fs::write(&path, bytes).unwrap();
        let directive = UpgradeDirective {
            upgrade_id: "up-1".to_string(),
            target_version: v("0.2.0"),
            binary_sha256: sha256_hex(bytes),
            embedded_signature: sign(&release_pkcs8, bytes).unwrap(),
            external_signature: None,
            source: BinarySource::LocalFile { path },
            network_provenance: false,
        };
        manager.prepare(&directive, vec![]).await.unwrap().unwrap();

        // The running (test) binary was copied in as the rollback target.
        assert!(binary_dir.join("bun-v0.1.0").is_file());
    }

    // M5: the single-node network flow downloads the artefact and stages it as
    // a LocalFile, so `source.is_network()` is false. It must still demand the
    // operator's external signature — otherwise anyone who can write the staged
    // file bypasses the second key that network upgrades exist to require.
    #[tokio::test]
    async fn network_provenance_local_file_still_requires_external_signature() {
        let fixture = fixture();
        let bytes = b"downloaded binary";
        let path = fixture.binary_dir.join("staged-download");
        std::fs::write(&path, bytes).unwrap();

        // A downloaded-then-staged binary with only the embedded signature: the
        // shape a compromised mirror or a stripped external signature produces.
        let unsigned = UpgradeDirective {
            upgrade_id: "net-1".to_string(),
            target_version: v("0.2.0"),
            binary_sha256: sha256_hex(bytes),
            embedded_signature: sign(&fixture.release_pkcs8, bytes).unwrap(),
            external_signature: None,
            source: BinarySource::LocalFile { path: path.clone() },
            network_provenance: true,
        };
        let err = fixture
            .manager
            .prepare(&unsigned, vec![])
            .await
            .unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalSignatureInvalid));

        // The same provenance with the external signature present is accepted.
        let signed = UpgradeDirective {
            external_signature: Some(sign(&fixture.external_pkcs8, bytes).unwrap()),
            ..unsigned
        };
        fixture
            .manager
            .prepare(&signed, vec![])
            .await
            .unwrap()
            .unwrap();
    }

    /// O3: the Pickle fetch URL was a hardcoded `http://`, so a node could
    /// not pull a binary from a TLS-only registry at all. It now follows the
    /// cluster plane's own scheme.
    ///
    /// Driven through the real fetch against a closed port: the error carries
    /// the URL it tried, which is the observable we care about. Integrity is
    /// unaffected either way — the sha256 gate and embedded release signature
    /// are checked on every path.
    #[tokio::test]
    async fn a_pickle_binary_fetch_follows_the_cluster_scheme() {
        let fixture = fixture();
        let directive = UpgradeDirective {
            upgrade_id: "u1".to_string(),
            target_version: v("0.2.0"),
            binary_sha256: "abc123".to_string(),
            embedded_signature: String::new(),
            external_signature: None,
            source: BinarySource::Pickle {
                // Port 1 is reserved and never listening, so the fetch fails
                // fast without depending on anything being up.
                registry_address: "127.0.0.1:1".to_string(),
            },
            network_provenance: true,
        };

        let plaintext = fixture.manager.fetch_binary(&directive).await;
        match plaintext {
            Err(UpgradeError::FetchFailed { url, .. }) => {
                assert!(url.starts_with("http://127.0.0.1:1/v2/"), "got {url}");
            }
            other => panic!("expected a fetch failure, got {other:?}"),
        }

        let secure = fixture
            .manager
            .with_cluster_http(crate::cluster::ClusterHttp::secure(reqwest::Client::new()))
            .fetch_binary(&directive)
            .await;
        match secure {
            Err(UpgradeError::FetchFailed { url, .. }) => {
                assert!(url.starts_with("https://127.0.0.1:1/v2/"), "got {url}");
            }
            other => panic!("expected a fetch failure, got {other:?}"),
        }
    }

    /// B2: a Pickle binary fetch presents the internal service token as a
    /// bearer, so it authenticates against a routable registry
    /// (`require_read_auth`) instead of 401ing. Driven through a raw TCP
    /// capture server that records the request line and headers.
    #[tokio::test]
    async fn a_pickle_binary_fetch_carries_the_service_token_bearer() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await;
            request
        });

        let fixture = fixture();
        let manager = fixture
            .manager
            .with_bearer(Some("rbrg_service".to_string()));
        let directive = UpgradeDirective {
            upgrade_id: "u1".to_string(),
            target_version: v("0.2.0"),
            binary_sha256: "abc123".to_string(),
            embedded_signature: String::new(),
            external_signature: None,
            source: BinarySource::Pickle {
                registry_address: addr.to_string(),
            },
            network_provenance: true,
        };

        // The 404 makes the fetch fail, but the request has already been sent.
        let _ = manager.fetch_binary(&directive).await;

        let request = capture.await.unwrap().to_lowercase();
        assert!(
            request.contains("authorization: bearer rbrg_service"),
            "binary fetch did not carry the service-token bearer:\n{request}"
        );
    }

    #[test]
    fn derive_stem_handles_all_invocation_shapes() {
        assert_eq!(derive_stem(Path::new("/usr/local/bin/bun")), "bun");
        assert_eq!(derive_stem(Path::new("/usr/local/bin/bun-v0.2.0")), "bun");
        assert_eq!(derive_stem(Path::new("target/debug/bun")), "bun");
        // A dash that isn't a version suffix stays put.
        assert_eq!(derive_stem(Path::new("/opt/bun-vnext")), "bun-vnext");
    }
}

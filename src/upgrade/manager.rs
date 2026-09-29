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

/// How hard a node tries to fetch a binary whose source is unavailable
/// before it answers the directive with a transient refusal, and how long
/// any one attempt may take.
///
/// Everything here is short and bounded on purpose: the fetch runs while
/// the agent holds its command loop and the orchestrator waits on the HTTP
/// answer, so a registry that accepts and then hangs must not stall the
/// agent. Longer outages are the orchestrator's job, which re-sends the
/// directive for minutes (see `orchestrator::DIRECTIVE_RETRY_WINDOW`).
#[derive(Debug, Clone, Copy)]
struct FetchRetry {
    /// Stop retrying once another backoff would pass this much time.
    budget: std::time::Duration,
    /// The first wait; each later one doubles, up to `max_backoff`.
    initial_backoff: std::time::Duration,
    max_backoff: std::time::Duration,
    /// Connecting and receiving the response headers, per attempt.
    response_timeout: std::time::Duration,
    /// One whole attempt, headers and body.
    attempt_timeout: std::time::Duration,
    /// The whole fetch, every attempt and backoff included. No attempt is
    /// allowed to run past it, so this is a hard upper bound.
    ceiling: std::time::Duration,
}

/// Connect + headers in 5 s; a ~100 MB binary over a LAN in well under a
/// minute; the whole fetch, retries included, within 75 s.
const DEFAULT_FETCH_RETRY: FetchRetry = FetchRetry {
    budget: std::time::Duration::from_secs(10),
    initial_backoff: std::time::Duration::from_millis(500),
    max_backoff: std::time::Duration::from_secs(4),
    response_timeout: std::time::Duration::from_secs(5),
    attempt_timeout: std::time::Duration::from_secs(60),
    ceiling: std::time::Duration::from_secs(75),
};

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
    /// Hex SHA-256 of the running binary, hashed once on first use (see
    /// [`UpgradeManager::running_binary_sha256`]).
    running_sha256: std::sync::Arc<tokio::sync::OnceCell<String>>,
    /// Retry policy for an unavailable binary source.
    fetch_retry: FetchRetry,
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
            running_sha256: std::sync::Arc::new(tokio::sync::OnceCell::new()),
            fetch_retry: DEFAULT_FETCH_RETRY,
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

    /// Hex SHA-256 of the binary this process runs, or `None` if it can't
    /// be read.
    ///
    /// The store's file for the running version is the source (it is what
    /// the entry symlink exec'd, and [`BinaryStore::stage`] refuses to put
    /// different bytes under an existing version). A plain install with no
    /// store file yet falls back to the executable itself. Hashing a whole
    /// Bun binary is CPU work, so it runs on the blocking pool, once.
    pub async fn running_binary_sha256(&self) -> Option<String> {
        let stored = self.store.binary_path(&self.running_version);
        self.running_sha256
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || hash_running_binary(&stored))
                    .await
                    .ok()
                    .flatten()
                    .ok_or(())
            })
            .await
            .ok()
            .cloned()
    }

    /// Can this node accept a cluster (network) upgrade? Those directives
    /// fetch the binary from Pickle and so need the operator's external
    /// key to verify it; without one every directive is refused.
    pub fn accepts_network_upgrades(&self) -> bool {
        self.external_key.is_some()
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

        self.check_directive_target(directive).await?;

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

        super::compatibility::check_binary(
            bytes.clone(),
            self.store.symlink_path().parent().ok_or_else(|| {
                UpgradeError::IncompatibleBinary("binary directory is missing".into())
            })?,
        )
        .await?;

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
    pub async fn prepare_rollback(
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
        // Hashing a whole Bun binary is too slow for an async task, and one
        // read serves both the signature check and the compatibility check.
        let binary = self.store.binary_path(&target);
        let envelope_path = self.store.envelope_path(&target);
        let release_keys = self.release_keys.clone();
        let external_key = self.external_key;
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, UpgradeError> {
            let bytes = std::fs::read(&binary)?;
            if let Ok(envelope) = SignatureEnvelope::load(&envelope_path)
                && !envelope.embedded.is_empty()
            {
                signing::verify_binary(
                    &bytes,
                    &envelope,
                    &release_keys,
                    external_key.as_ref(),
                    envelope.external.is_some(),
                )?;
            }
            Ok(bytes)
        })
        .await
        .map_err(|error| UpgradeError::IncompatibleBinary(error.to_string()))??;
        super::compatibility::check_binary(
            bytes,
            self.store.binary_path(&target).parent().ok_or_else(|| {
                UpgradeError::IncompatibleBinary("binary directory is missing".into())
            })?,
        )
        .await?;

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

    /// Refuse a same-version or unrequested downgrade directive before
    /// fetching anything. Identical bytes on the same version come back as
    /// [`UpgradeError::AlreadyRunning`] so the caller can say "nothing to do".
    async fn check_directive_target(
        &self,
        directive: &UpgradeDirective,
    ) -> Result<(), UpgradeError> {
        // Only a same-version directive needs the (hashed) running digest.
        let sha256 = if directive.target_version == self.running_version {
            self.running_binary_sha256().await
        } else {
            None
        };
        let running = super::plan::RunningBinary {
            node: "this node".to_string(),
            version: self.running_version.clone(),
            sha256,
        };
        match super::plan::check_target(
            &directive.target_version,
            &directive.binary_sha256,
            directive.allow_downgrade,
            &[running],
        )? {
            super::plan::TargetCheck::Proceed => Ok(()),
            super::plan::TargetCheck::AlreadyRunning => Err(UpgradeError::AlreadyRunning {
                version: directive.target_version.clone(),
            }),
        }
    }

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
                self.fetch_with_retry(&url).await
            }
        }
    }

    /// Fetch `url`, riding out a source that is briefly unavailable (a
    /// registry restarting with its node) for up to the retry budget.
    /// A permanent failure returns at once; nothing runs past the ceiling.
    async fn fetch_with_retry(&self, url: &str) -> Result<Vec<u8>, UpgradeError> {
        let started = tokio::time::Instant::now();
        let mut backoff = self.fetch_retry.initial_backoff;
        loop {
            let remaining = self.fetch_retry.ceiling.saturating_sub(started.elapsed());
            let attempt_timeout = self.fetch_retry.attempt_timeout.min(remaining);
            let error = match tokio::time::timeout(attempt_timeout, self.fetch_once(url)).await {
                Ok(Ok(bytes)) => return Ok(bytes),
                Ok(Err(error)) => error,
                Err(_) => UpgradeError::FetchUnavailable {
                    url: url.to_string(),
                    reason: format!(
                        "no complete response within {}ms",
                        attempt_timeout.as_millis()
                    ),
                },
            };
            if !error.is_transient() || started.elapsed() + backoff > self.fetch_retry.budget {
                return Err(error);
            }
            eprintln!(
                "bun: {error}; retrying the binary fetch in {}ms",
                backoff.as_millis()
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.fetch_retry.max_backoff);
        }
    }

    /// One GET of the binary, classified: connection trouble, a cut-off
    /// body and 5xx/408/429 are [`UpgradeError::FetchUnavailable`]; any
    /// other non-success status is [`UpgradeError::FetchFailed`].
    async fn fetch_once(&self, url: &str) -> Result<Vec<u8>, UpgradeError> {
        let unavailable = |reason: String| UpgradeError::FetchUnavailable {
            url: url.to_string(),
            reason,
        };
        // `get` carries the internal service token as a bearer: on a
        // routable cluster the registry sets `require_read_auth`, so a
        // bearer-less binary fetch 401s (B2).
        let response = tokio::time::timeout(
            self.fetch_retry.response_timeout,
            self.cluster_http.get(url).send(),
        )
        .await
        .map_err(|_| {
            unavailable(format!(
                "no response headers within {}ms",
                self.fetch_retry.response_timeout.as_millis()
            ))
        })?
        .map_err(|e| unavailable(e.to_string()))?;
        let status = response.status();
        if super::is_transient_status(status) {
            return Err(unavailable(format!("status {status}")));
        }
        if !status.is_success() {
            return Err(UpgradeError::FetchFailed {
                url: url.to_string(),
                reason: format!("status {status}"),
            });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        Ok(bytes.to_vec())
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

/// Hash the running binary: the store's copy if present, else the
/// executable this process was started from.
fn hash_running_binary(stored: &Path) -> Option<String> {
    let bytes = match std::fs::read(stored) {
        Ok(bytes) => bytes,
        Err(_) => std::fs::read(running_executable()?).ok()?,
    };
    Some(signing::sha256_hex(&bytes))
}

/// The executable this process runs. On Linux `/proc/self/exe` opens the
/// exact inode even if the path was replaced since exec.
fn running_executable() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        return Some(PathBuf::from("/proc/self/exe"));
    }
    std::env::current_exe().ok()
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
        let mut manager = UpgradeManager::new(
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
        // Same retry behaviour, a test-sized budget.
        manager.fetch_retry = FetchRetry {
            budget: std::time::Duration::from_millis(300),
            initial_backoff: std::time::Duration::from_millis(10),
            max_backoff: std::time::Duration::from_millis(50),
            response_timeout: std::time::Duration::from_millis(200),
            attempt_timeout: std::time::Duration::from_millis(500),
            ceiling: std::time::Duration::from_secs(1),
        };

        Fixture {
            _dir: dir,
            manager,
            release_pkcs8,
            external_pkcs8,
            binary_dir,
        }
    }

    fn compatible_binary(label: &[u8]) -> Vec<u8> {
        let formats = serde_json::to_string(&crate::compatibility::CURRENT).unwrap();
        let mut binary = format!("#!/bin/sh\nprintf '%s' '{formats}'\n# ").into_bytes();
        binary.extend_from_slice(label);
        binary.push(b'\n');
        binary
    }

    fn directive_for(fixture: &Fixture, bytes: &[u8], id: &str) -> UpgradeDirective {
        let executable = if bytes.starts_with(b"#!/") {
            bytes.to_vec()
        } else {
            compatible_binary(bytes)
        };
        let bytes = executable.as_slice();
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
            allow_downgrade: false,
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
    async fn signed_incompatible_binary_is_refused_before_staging() {
        let fixture = fixture();
        let directive = directive_for(
            &fixture,
            b"#!/bin/sh\nprintf '%s' '{\"protocol\":1,\"state\":1}'\n",
            "incompatible",
        );
        assert!(fixture.manager.prepare(&directive, vec![]).await.is_err());
        assert!(!fixture.manager.upgrade_in_flight());
        assert!(!fixture.manager.store().binary_path(&v("0.2.0")).exists());
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );
    }

    #[tokio::test]
    async fn running_binary_sha256_hashes_the_stored_running_version() {
        let fixture = fixture();
        assert_eq!(
            fixture.manager.running_binary_sha256().await.as_deref(),
            Some(sha256_hex(b"old binary").as_str())
        );
    }

    #[tokio::test]
    async fn same_version_directive_with_different_bytes_is_refused_untouched() {
        let fixture = fixture();
        let mut directive = directive_for(&fixture, b"rebuilt binary", "same-version");
        directive.target_version = v("0.1.0");

        let err = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(
            matches!(err, UpgradeError::SameVersionDifferentBinary { .. }),
            "{err}"
        );
        assert!(!fixture.manager.upgrade_in_flight());
        // The running version's bytes are still the original ones.
        assert_eq!(
            std::fs::read(fixture.manager.store().binary_path(&v("0.1.0"))).unwrap(),
            b"old binary"
        );
    }

    #[tokio::test]
    async fn same_version_directive_with_identical_bytes_reports_already_running() {
        let fixture = fixture();
        let mut directive = directive_for(&fixture, b"unused", "same-bytes");
        directive.target_version = v("0.1.0");
        directive.binary_sha256 = sha256_hex(b"old binary");

        let err = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(matches!(err, UpgradeError::AlreadyRunning { .. }), "{err}");
        assert!(!fixture.manager.upgrade_in_flight());
    }

    #[tokio::test]
    async fn downgrade_directive_needs_allow_downgrade() {
        let fixture = fixture();
        let mut directive = directive_for(&fixture, b"soak build", "downgrade");
        directive.target_version = v("0.1.0-soak.1");

        let err = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();
        assert!(
            matches!(err, UpgradeError::DowngradeRefused { .. }),
            "{err}"
        );
        assert!(!fixture.manager.upgrade_in_flight());

        directive.allow_downgrade = true;
        let prepared = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap()
            .expect("an allowed downgrade stages");
        assert_eq!(prepared.target_version(), &v("0.1.0-soak.1"));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn verified_upgrade_probes_survive_concurrent_process_creation() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut churn = Vec::new();
        for _ in 0..4 {
            let shutdown = shutdown.clone();
            churn.push(tokio::spawn(async move {
                while !shutdown.is_cancelled() {
                    let status = tokio::process::Command::new("true")
                        .kill_on_drop(true)
                        .status()
                        .await
                        .unwrap();
                    assert!(status.success());
                }
            }));
        }
        let results = futures_util::future::join_all((0..32).map(|_| async {
            let fixture = fixture();
            let directive = directive_for(&fixture, b"concurrent probe", "concurrent");
            fixture
                .manager
                .prepare(&directive, vec![])
                .await
                .map(|prepared| {
                    assert!(prepared.is_some());
                    assert!(fixture.manager.upgrade_in_flight());
                })
        }))
        .await;
        shutdown.cancel();
        for task in churn {
            task.await.unwrap();
        }
        for result in results {
            result.unwrap();
        }
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
            .await
            .unwrap_err();
        assert!(matches!(err, UpgradeError::UnknownVersion { .. }));
    }

    #[tokio::test]
    async fn rollback_refuses_a_development_binary_without_a_marker() {
        let fixture = fixture();
        let stub = SignatureEnvelope {
            schema: 1,
            sha256: String::new(),
            embedded: String::new(),
            external: None,
        };
        fixture
            .manager
            .store()
            .stage(
                &v("0.0.9"),
                b"#!/bin/sh\nprintf '%s' '{\"protocol\":1,\"state\":1}'\n",
                &stub,
            )
            .unwrap();
        assert!(matches!(
            fixture.manager.prepare_rollback(None, vec![]).await,
            Err(UpgradeError::IncompatibleBinary(_))
        ));
        assert!(!fixture.manager.upgrade_in_flight());
        assert_eq!(
            fixture.manager.store().current_target().unwrap(),
            v("0.1.0")
        );
    }

    #[tokio::test]
    async fn compatibility_query_timeout_leaves_no_marker_or_candidate() {
        let fixture = fixture();
        let directive = directive_for(&fixture, b"#!/bin/sh\nexec sleep 30\n", "stalled");
        let started = std::time::Instant::now();
        // The query deadline is a Tokio timer. With the clock paused it
        // expires as soon as the runtime idles on the silent candidate, so
        // the test proves the bound without spending ten real seconds.
        tokio::time::pause();
        let prepared = fixture.manager.prepare(&directive, vec![]).await;
        tokio::time::resume();
        // Only the deadline yields `Elapsed`; a spawn or exit failure would
        // be a different incompatibility and must not pass as a timeout.
        assert!(
            matches!(&prepared, Err(UpgradeError::IncompatibleBinary(message)) if message.contains("Elapsed")),
            "{prepared:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        assert!(!fixture.manager.upgrade_in_flight());
        assert!(!fixture.manager.store().binary_path(&v("0.2.0")).exists());
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
            store
                .stage(&v(version), &compatible_binary(b"rollback"), &stub)
                .unwrap();
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

        let prepared = manager.prepare_rollback(None, vec![]).await.unwrap();
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

        let err = manager.prepare_rollback(None, vec![]).await.unwrap_err();
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

        let executable = compatible_binary(b"new binary");
        let bytes = executable.as_slice();
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
            allow_downgrade: false,
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
        let executable = compatible_binary(b"downloaded binary");
        let bytes = executable.as_slice();
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
            allow_downgrade: false,
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
            allow_downgrade: false,
        };

        let plaintext = fixture.manager.fetch_binary(&directive).await;
        match plaintext {
            Err(UpgradeError::FetchUnavailable { url, .. }) => {
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
            Err(UpgradeError::FetchUnavailable { url, .. }) => {
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
            allow_downgrade: false,
        };

        // The 404 makes the fetch fail, but the request has already been sent.
        let _ = manager.fetch_binary(&directive).await;

        let request = capture.await.unwrap().to_lowercase();
        assert!(
            request.contains("authorization: bearer rbrg_service"),
            "binary fetch did not carry the service-token bearer:\n{request}"
        );
    }

    /// What a [`flaky_registry`] does with one request.
    #[derive(Clone, Copy)]
    enum RegistryAnswer {
        /// Close the connection without answering (a registry mid-restart).
        Hangup,
        /// Answer with this status line and no body.
        Status(&'static str),
        /// Answer 200 with the blob.
        Blob,
        /// Accept the request and never answer.
        Silent,
        /// Send the headers and half the blob, then stall.
        StallMidBody,
    }

    /// A registry that gives `script`'s answers in order, then serves the
    /// blob. Returns its address and a count of requests it saw.
    async fn flaky_registry(
        script: Vec<RegistryAnswer>,
        blob: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let index = seen.fetch_add(1, Ordering::SeqCst);
                let answer = script.get(index).copied().unwrap_or(RegistryAnswer::Blob);
                match answer {
                    RegistryAnswer::Hangup => drop(socket),
                    RegistryAnswer::Status(line) => {
                        let response = format!(
                            "HTTP/1.1 {line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                    RegistryAnswer::Silent => {
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                            drop(socket);
                        });
                    }
                    RegistryAnswer::StallMidBody => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            blob.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&blob[..blob.len() / 2]).await;
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                            drop(socket);
                        });
                    }
                    RegistryAnswer::Blob => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            blob.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&blob).await;
                    }
                }
            }
        });
        (address, requests)
    }

    /// A directive for a properly signed binary served by `registry`.
    fn pickle_directive(fixture: &Fixture, bytes: &[u8], registry: &str) -> UpgradeDirective {
        UpgradeDirective {
            source: BinarySource::Pickle {
                registry_address: registry.to_string(),
            },
            network_provenance: true,
            ..directive_for(fixture, bytes, "pickle-1")
        }
    }

    /// The V02 soak failure, node side: the leader restarted, and a worker
    /// asked its registry for the binary before it was listening again.
    #[tokio::test]
    async fn prepare_rides_out_a_registry_that_is_briefly_unavailable() {
        let fixture = fixture();
        let binary = compatible_binary(b"from a restarting registry");
        let (registry, requests) = flaky_registry(
            vec![
                RegistryAnswer::Hangup,
                RegistryAnswer::Status("503 Service Unavailable"),
            ],
            binary.clone(),
        )
        .await;
        let directive = pickle_directive(&fixture, &binary, &registry);

        let prepared = fixture.manager.prepare(&directive, vec![]).await.unwrap();

        assert!(prepared.is_some());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_blob_the_registry_does_not_hold_is_refused_without_retrying() {
        let fixture = fixture();
        let binary = compatible_binary(b"never pushed");
        let (registry, requests) = flaky_registry(
            vec![RegistryAnswer::Status("404 Not Found")],
            binary.clone(),
        )
        .await;
        let directive = pickle_directive(&fixture, &binary, &registry);

        let error = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(matches!(error, UpgradeError::FetchFailed { .. }), "{error}");
        assert!(!error.is_transient());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_registry_down_for_the_whole_budget_is_a_transient_failure() {
        let fixture = fixture();
        let binary = compatible_binary(b"registry never comes back");
        let (registry, requests) =
            flaky_registry(vec![RegistryAnswer::Hangup; 1000], binary.clone()).await;
        let directive = pickle_directive(&fixture, &binary, &registry);

        let error = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(
            matches!(error, UpgradeError::FetchUnavailable { .. }),
            "{error}"
        );
        assert!(error.is_transient());
        assert!(requests.load(std::sync::atomic::Ordering::SeqCst) > 1);
        // Nothing staged: the running system is untouched.
        assert!(!fixture.binary_dir.join("bun-v0.2.0").exists());
    }

    /// A registry that accepts and then hangs (before the headers, or
    /// halfway through the body) must not hold the agent's command loop:
    /// every attempt times out, and the whole fetch ends by the ceiling.
    #[tokio::test]
    async fn a_hanging_registry_is_a_transient_failure_within_the_ceiling() {
        for answer in [RegistryAnswer::Silent, RegistryAnswer::StallMidBody] {
            let fixture = fixture();
            let binary = compatible_binary(b"a registry that hangs");
            let (registry, requests) = flaky_registry(vec![answer; 1000], binary.clone()).await;
            let directive = pickle_directive(&fixture, &binary, &registry);

            let started = std::time::Instant::now();
            let error = fixture
                .manager
                .prepare(&directive, vec![])
                .await
                .unwrap_err();
            let took = started.elapsed();

            assert!(
                matches!(error, UpgradeError::FetchUnavailable { .. }),
                "{error}"
            );
            // The test ceiling is 1 s; allow scheduling slack, not a hang.
            assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
            assert!(requests.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        }
    }

    /// Bytes that fail verification are a refusal, even though they came
    /// from a registry: retrying would fetch the same bytes again.
    #[tokio::test]
    async fn a_binary_that_fails_verification_is_not_transient() {
        let fixture = fixture();
        let binary = compatible_binary(b"the signed one");
        let (registry, _) = flaky_registry(Vec::new(), compatible_binary(b"something else")).await;
        let directive = pickle_directive(&fixture, &binary, &registry);

        let error = fixture
            .manager
            .prepare(&directive, vec![])
            .await
            .unwrap_err();

        assert!(!error.is_transient(), "{error}");
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

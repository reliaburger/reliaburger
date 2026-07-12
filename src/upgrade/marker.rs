//! The upgrade marker and the startup decision state machine.
//!
//! Node-level upgrade state must survive `exec()` and crashes, so it lives
//! in a marker file at `{data_dir}/upgrade/marker.json` — Raft and the rest
//! of the runtime are not available in the moments that matter. The marker
//! exists only while an upgrade is in flight on this node; a committed
//! upgrade DELETES it (no marker == nothing in flight, one source of truth).
//!
//! Every bun startup begins by loading the marker (if any) and running
//! [`decide_startup`], a pure function that classifies the situation. The
//! decision drives everything: verify the new version, revert to the old
//! one, complete a revert, or just boot.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::UpgradeError;
use super::version::BinaryVersion;

/// Marker file name within `{data_dir}/upgrade/`.
pub const MARKER_FILE: &str = "marker.json";

/// One workload instance recorded before the swap, for post-upgrade
/// self-verification ("did everything survive?").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInventory {
    pub namespace: String,
    pub app_name: String,
    pub instance_id: u32,
    pub pid: u32,
    /// The instance's full canonical id string. Defaults to empty when a
    /// marker written by a pre-theme binary is loaded across an upgrade;
    /// the reconstruction path falls back to the legacy `{app}-{ordinal}`
    /// form in that case.
    #[serde(default)]
    pub full_id: String,
}

/// Node-local upgrade state, persisted across exec and crashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeMarker {
    /// Marker format version. Currently 1.
    pub schema: u32,
    /// Idempotency key from the directive that started this upgrade.
    pub upgrade_id: String,
    /// The version that was running when the upgrade started.
    pub previous_version: BinaryVersion,
    /// File name of the previous binary within the store, e.g. `bun-v0.1.0`.
    pub previous_binary: String,
    /// The version being upgraded to.
    pub target_version: BinaryVersion,
    /// File name of the target binary within the store.
    pub target_binary: String,
    /// Where the node-level upgrade currently stands.
    pub phase: MarkerPhase,
    /// How many times a binary has booted with this marker present.
    /// Incremented by each post-swap startup; beyond the configured
    /// maximum, startup reverts instead of retrying.
    pub boot_attempts: u32,
    /// Workloads that were running just before the swap.
    pub pre_upgrade_instances: Vec<InstanceInventory>,
}

/// Node-level upgrade phase.
///
/// There is deliberately no `Committed` variant: committing an upgrade
/// deletes the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerPhase {
    /// Binary and signature staged; the symlink has NOT been swapped yet.
    Staged,
    /// Symlink swapped and exec called (written just before `execv`).
    Executed,
    /// The new binary booted and is running self-verification.
    Verifying,
    /// Verification failed or the new binary crash-looped: revert on the
    /// next startup.
    RevertPending,
    /// The previous binary is back in control; the failure still needs
    /// reporting before the marker is archived.
    Reverted,
}

impl UpgradeMarker {
    /// Path of the marker within a data directory.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("upgrade").join(MARKER_FILE)
    }

    /// Load the marker if one exists.
    ///
    /// A missing file is `Ok(None)` (the normal case). An unreadable or
    /// unparseable file is an error — the caller archives it as stale
    /// rather than guessing.
    pub fn load(path: &Path) -> Result<Option<Self>, UpgradeError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let marker = serde_json::from_str(&contents).map_err(|e| UpgradeError::InvalidMarker {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        Ok(Some(marker))
    }

    /// Persist the marker atomically (temp file + rename + directory fsync),
    /// so a crash mid-write leaves the previous marker intact.
    pub fn store(&self, path: &Path) -> Result<(), UpgradeError> {
        let dir = path.parent().ok_or_else(|| UpgradeError::InvalidMarker {
            path: path.to_path_buf(),
            reason: "marker path has no parent directory".to_string(),
        })?;
        std::fs::create_dir_all(dir)?;

        let tmp = dir.join(format!(".{MARKER_FILE}.tmp-{}", std::process::id()));
        // A struct of strings and integers always serialises.
        let json = serde_json::to_string_pretty(self).expect("marker serialises");
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }

    /// Delete the marker (commit or archive completion).
    pub fn remove(path: &Path) -> Result<(), UpgradeError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Move a stale/inconsistent marker aside (`marker.json.stale-N`) so it
    /// never blocks future upgrades but stays available for diagnosis.
    pub fn archive_stale(path: &Path) -> Result<PathBuf, UpgradeError> {
        for n in 1..=99u32 {
            let candidate = path.with_file_name(format!("{MARKER_FILE}.stale-{n}"));
            if !candidate.exists() {
                std::fs::rename(path, &candidate)?;
                return Ok(candidate);
            }
        }
        // 99 stale markers means something is very wrong; stop hiding it.
        Err(UpgradeError::InvalidMarker {
            path: path.to_path_buf(),
            reason: "too many stale markers archived; clean {data_dir}/upgrade".to_string(),
        })
    }
}

/// What a bun startup should do about upgrade state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupDecision {
    /// No marker: normal boot.
    NormalBoot,
    /// We are the freshly swapped-in target version (within the boot-attempt
    /// budget): boot, then run self-verification. The marker carried here
    /// has `boot_attempts` already incremented and `phase: Verifying`; the
    /// caller must persist it before proceeding.
    VerifyUpgrade { marker: UpgradeMarker },
    /// We are the target version but have crash-looped past the budget:
    /// revert the symlink to `previous_binary` and exec it. The marker
    /// carried here has `phase: RevertPending`; persist before acting.
    RevertAndExecPrevious { marker: UpgradeMarker },
    /// We are the PREVIOUS version, back in control after a failed upgrade:
    /// report the failure, archive the marker, and continue booting.
    CompleteRevert { marker: UpgradeMarker },
    /// The marker doesn't describe a state this binary can act on: archive
    /// it with a warning and boot normally. Never brick the node over
    /// bookkeeping.
    ArchiveStaleMarker { reason: String },
}

/// Classify the startup situation. Pure function: all filesystem access
/// happens before (marker load) and after (persisting the returned marker).
pub fn decide_startup(
    marker: Option<UpgradeMarker>,
    running: &BinaryVersion,
    max_boot_attempts: u32,
) -> StartupDecision {
    let Some(marker) = marker else {
        return StartupDecision::NormalBoot;
    };

    let is_target = *running == marker.target_version;
    let is_previous = *running == marker.previous_version;

    match marker.phase {
        // Staged but never swapped: the process died between staging and
        // activation. The symlink still points at the previous version,
        // which is what's running. Nothing was changed; forget the attempt.
        MarkerPhase::Staged => StartupDecision::ArchiveStaleMarker {
            reason: format!("upgrade {} staged but never executed", marker.upgrade_id),
        },

        // The swap happened (or verification was already in progress) and
        // the target version is running: count the boot and either verify
        // or give up and revert.
        MarkerPhase::Executed | MarkerPhase::Verifying if is_target => {
            let mut marker = marker;
            marker.boot_attempts += 1;
            if marker.boot_attempts > max_boot_attempts {
                marker.phase = MarkerPhase::RevertPending;
                StartupDecision::RevertAndExecPrevious { marker }
            } else {
                marker.phase = MarkerPhase::Verifying;
                StartupDecision::VerifyUpgrade { marker }
            }
        }

        // The swap happened but the PREVIOUS version is running: an
        // operator (or a revert that never updated the phase) put the old
        // binary back. Treat it as a completed revert so the failure gets
        // reported rather than silently forgotten.
        MarkerPhase::Executed | MarkerPhase::Verifying if is_previous => {
            StartupDecision::CompleteRevert { marker }
        }

        // Revert was decided; the previous version is back: complete it.
        MarkerPhase::RevertPending | MarkerPhase::Reverted if is_previous => {
            StartupDecision::CompleteRevert { marker }
        }

        // Revert was decided but the target is somehow running again
        // (supervisor raced the symlink change): revert again.
        MarkerPhase::RevertPending | MarkerPhase::Reverted if is_target => {
            StartupDecision::RevertAndExecPrevious { marker }
        }

        // Running a version the marker doesn't mention at all.
        _ => StartupDecision::ArchiveStaleMarker {
            reason: format!(
                "running {running} but marker describes {} -> {}",
                marker.previous_version, marker.target_version
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    fn marker(phase: MarkerPhase, boot_attempts: u32) -> UpgradeMarker {
        UpgradeMarker {
            schema: 1,
            upgrade_id: "up-1".to_string(),
            previous_version: v("0.1.0"),
            previous_binary: "bun-v0.1.0".to_string(),
            target_version: v("0.2.0"),
            target_binary: "bun-v0.2.0".to_string(),
            phase,
            boot_attempts,
            pre_upgrade_instances: vec![InstanceInventory {
                namespace: "default".to_string(),
                app_name: "web".to_string(),
                instance_id: 0,
                pid: 4242,
                full_id: "default__web-0".to_string(),
            }],
        }
    }

    const MAX: u32 = 2;

    #[test]
    fn pre_theme_marker_without_full_id_still_loads() {
        // A marker written by a pre-identity-theme binary has no `full_id`
        // on its instance inventory. It must deserialise cleanly (serde
        // default), so an in-flight upgrade across the change is not
        // orphaned.
        let json = r#"{
            "schema": 1,
            "upgrade_id": "up-1",
            "previous_version": "0.1.0",
            "previous_binary": "bun-v0.1.0",
            "target_version": "0.2.0",
            "target_binary": "bun-v0.2.0",
            "phase": "Executed",
            "boot_attempts": 0,
            "pre_upgrade_instances": [
                {"namespace":"default","app_name":"web","instance_id":0,"pid":4242}
            ]
        }"#;
        let marker: UpgradeMarker = serde_json::from_str(json).expect("legacy marker loads");
        let inst = &marker.pre_upgrade_instances[0];
        assert_eq!(inst.full_id, "", "missing full_id defaults to empty");
        assert_eq!(inst.app_name, "web");
        assert_eq!(inst.instance_id, 0);
    }

    #[test]
    fn no_marker_boots_normally() {
        assert_eq!(
            decide_startup(None, &v("0.1.0"), MAX),
            StartupDecision::NormalBoot
        );
    }

    #[test]
    fn fresh_upgrade_enters_verification() {
        let decision = decide_startup(Some(marker(MarkerPhase::Executed, 0)), &v("0.2.0"), MAX);
        let StartupDecision::VerifyUpgrade { marker } = decision else {
            panic!("expected VerifyUpgrade, got {decision:?}");
        };
        assert_eq!(marker.boot_attempts, 1);
        assert_eq!(marker.phase, MarkerPhase::Verifying);
    }

    #[test]
    fn second_crash_still_verifies_when_under_limit() {
        let decision = decide_startup(Some(marker(MarkerPhase::Verifying, 1)), &v("0.2.0"), MAX);
        let StartupDecision::VerifyUpgrade { marker } = decision else {
            panic!("expected VerifyUpgrade, got {decision:?}");
        };
        assert_eq!(marker.boot_attempts, 2);
    }

    #[test]
    fn crash_loop_beyond_limit_reverts() {
        let decision = decide_startup(Some(marker(MarkerPhase::Verifying, 2)), &v("0.2.0"), MAX);
        let StartupDecision::RevertAndExecPrevious { marker } = decision else {
            panic!("expected RevertAndExecPrevious, got {decision:?}");
        };
        assert_eq!(marker.boot_attempts, 3);
        assert_eq!(marker.phase, MarkerPhase::RevertPending);
    }

    #[test]
    fn previous_binary_after_revert_completes_the_revert() {
        for phase in [MarkerPhase::RevertPending, MarkerPhase::Reverted] {
            let decision = decide_startup(Some(marker(phase, 3)), &v("0.1.0"), MAX);
            assert!(
                matches!(decision, StartupDecision::CompleteRevert { .. }),
                "phase {phase:?} gave {decision:?}"
            );
        }
    }

    #[test]
    fn previous_binary_running_after_swap_completes_the_revert() {
        // Operator manually restored the old binary while the marker still
        // says Executed/Verifying: report the failure, don't retry.
        for phase in [MarkerPhase::Executed, MarkerPhase::Verifying] {
            let decision = decide_startup(Some(marker(phase, 1)), &v("0.1.0"), MAX);
            assert!(
                matches!(decision, StartupDecision::CompleteRevert { .. }),
                "phase {phase:?} gave {decision:?}"
            );
        }
    }

    #[test]
    fn target_running_with_revert_pending_reverts_again() {
        let decision = decide_startup(
            Some(marker(MarkerPhase::RevertPending, 3)),
            &v("0.2.0"),
            MAX,
        );
        assert!(matches!(
            decision,
            StartupDecision::RevertAndExecPrevious { .. }
        ));
    }

    #[test]
    fn staged_but_never_swapped_is_archived_stale() {
        let decision = decide_startup(Some(marker(MarkerPhase::Staged, 0)), &v("0.1.0"), MAX);
        assert!(matches!(
            decision,
            StartupDecision::ArchiveStaleMarker { .. }
        ));
    }

    #[test]
    fn version_matching_neither_side_is_archived_stale() {
        let decision = decide_startup(Some(marker(MarkerPhase::Executed, 0)), &v("0.3.0"), MAX);
        assert!(matches!(
            decision,
            StartupDecision::ArchiveStaleMarker { .. }
        ));
    }

    #[test]
    fn marker_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = UpgradeMarker::path_in(dir.path());
        let original = marker(MarkerPhase::Executed, 1);
        original.store(&path).unwrap();
        assert_eq!(UpgradeMarker::load(&path).unwrap(), Some(original));
    }

    #[test]
    fn missing_marker_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = UpgradeMarker::path_in(dir.path());
        assert_eq!(UpgradeMarker::load(&path).unwrap(), None);
    }

    #[test]
    fn corrupt_marker_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = UpgradeMarker::path_in(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "definitely not json").unwrap();
        let err = UpgradeMarker::load(&path).unwrap_err();
        assert!(matches!(err, UpgradeError::InvalidMarker { .. }));
    }

    #[test]
    fn archive_stale_moves_marker_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = UpgradeMarker::path_in(dir.path());
        marker(MarkerPhase::Staged, 0).store(&path).unwrap();

        let archived = UpgradeMarker::archive_stale(&path).unwrap();

        assert!(!path.exists());
        assert!(archived.ends_with("marker.json.stale-1"));
        assert!(archived.exists());

        // A second stale marker gets the next suffix.
        marker(MarkerPhase::Staged, 0).store(&path).unwrap();
        let archived2 = UpgradeMarker::archive_stale(&path).unwrap();
        assert!(archived2.ends_with("marker.json.stale-2"));
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = UpgradeMarker::path_in(dir.path());
        marker(MarkerPhase::Executed, 1).store(&path).unwrap();
        UpgradeMarker::remove(&path).unwrap();
        UpgradeMarker::remove(&path).unwrap(); // already gone: still fine
        assert!(!path.exists());
    }
}

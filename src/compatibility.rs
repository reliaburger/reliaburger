//! Explicit boundaries for supported cluster protocols and durable state.

use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Formats understood by one binary. Equality is the initial rolling-upgrade policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Cluster protocol generation, independent of the product version.
    pub protocol: u32,
    /// Durable state generation, including Raft snapshots and sealed backups.
    pub state: u32,
}

/// Supported formats, including the per-node `directive_retry` record in a
/// cluster upgrade, the 503 a node answers a directive with when the
/// binary's registry is unavailable (the orchestrator retries it), and the
/// nodes each app last ran on (`DesiredState::last_placed_nodes`), the
/// log store's ingest checkpoint, which records each capture file's device
/// and inode beside its offset, the volume snapshot layout (reversible
/// volume slugs, per-destination export receipts, restore journals and
/// volume quotas in the sidecar), and each image index entry's platform in
/// Pickle's catalogue (`LayerDescriptor::platform`) and image listings
/// (`ImageSummary::platforms`), the apps a namespace quota keeps
/// unplaced (`RaftRequest::QuotaBlocked`, `DesiredState::quota_blocked` and
/// `DesiredAppEvidence::blocked`), the cluster-wide views: peers answer
/// deploy history, events and jobs for themselves on `local=true` and the
/// WebSocket log stream sends `LogFrame` JSON (F07); the volume home a
/// managed-volume app waits for (`DesiredAppEvidence::volume_home_away`, #423);
/// the recovery fence (#424): gossip advertises each node's recovery
/// epoch, leader hints order by epoch then term, and a Raft peer refuses a
/// fenced RPC with a `Fenced` reply; and cluster-wide instance ordinals:
/// each placement's `Placement::ordinal` in council state, the ordinals a
/// placement poll hands a node (`NodeAssignment::ordinals`) and the
/// instance ids named after them (#398), and the `token`, `secret` and
/// `identity` event kinds a peer's `/v1/events` answer can carry (F05 I1),
/// and the API token expiry sweep (`RaftRequest::SweepExpiredApiTokens`,
/// `CouncilResponse::ApiTokensSwept`) with the token list each peer answers
/// on `local=true`, carrying scope and last use (F05 I2), and the per-blob
/// repository upload receipts, keyed by repository and exact lease generation
/// before scoped publication can reuse shared CAS content (#531).
/// Complete desired-spec fingerprints and namespace-qualified preview keys (#550).
/// Node-owned remote metrics prefixes and persisted plaintext archive ownership (#533).
/// Batch execution identities, current-attempt reports and trusted label maps (#535).
/// Durable owned attempts, compact retired proofs and retained execution ownership (#535).
/// Shared batch requests and whole-pass placement admission revisions (#543).
/// Replicated migration/job intent and original-generation settlement fences (#534).
/// Authenticated webhook trigger admission and delivery receipts in durable Raft (#553).
/// Owned metrics publication metadata and immutable log ingestion checkpoints (#555).
/// Instance adoption records name their kernel boot and, on Linux, a process start in clock ticks since boot (#607).
/// Per-namespace secret keys: `NamespaceSpec::secret_key` and the values a
/// namespace's first key re-seals (`RaftRequest::RotateSecretKey::resealed`)
/// (F05 I4, #363).
/// Several CAs per role: each CA's `CaState`, the node leaf records
/// (`SecurityState::node_leaves`) and the `CaRotationBegin` and
/// `CaRotationFinalize` Raft requests (F04 R1, #362).
/// Trust bundles: the trusted Node CAs and roots in a join bundle
/// (`JoinBundle::trusted_node_cas_b64`, `trusted_roots_b64`) and in
/// `GET /v1/cluster/ca`, the workload CA bundle in a signing answer
/// (`WorkloadCsrResponse::ca_bundle_der`), and the trust set in a node's
/// identity snapshot (`node.bundle.json` schema 3) (F04 R2, #362).
/// API token rotation: `RaftRequest::RotateApiToken` and each stored token's
/// `ApiToken::previous_secret`, the old secret during its grace period (F05 I3).
/// Image references bound to digests at apply (`nginx:1.27@sha256:…`) in app
/// and job specs, deploy history and prerequisite claims, and the tag a bound
/// pull records in the pull-through cache (F03 U1, #361).
/// Intermediate rotation: the pending CSRs (`SecurityState::pending_intermediates`),
/// each node's trust acknowledgement (`NodeLeafRecord::trust_generation`),
/// the `CaRotationPrepare` and `AcknowledgeNodeTrust` Raft requests and the
/// `POST /v1/cluster/trust-ack` body a node sends the leader (F04 R4, #362).
/// and task arrays (`RaftRequest::TaskArray`, `DesiredState::task_arrays`
/// and node ledgers under `task-arrays/`), including mixed-profile manifests,
/// persistent recovery/term/index fences, 64-bit grant generations, accepted
/// ownership ranges, duration buckets and indexed result pages (47/64).
pub const CURRENT: Compatibility = Compatibility {
    protocol: 47,
    state: 64,
};

/// Name of the durable format stamp at the root of a node's data directory.
pub const STATE_STAMP: &str = "state-format.json";

/// Where the compatibility policy every refusal points to lives.
pub const POLICY_URL: &str = "https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#compatibility-before-100";

/// This binary as a refusal names it: `reliaburger v0.1.1 (3fcb1fd)`, or
/// just the version when the build didn't know its commit.
pub(crate) fn this_binary() -> String {
    use crate::upgrade::version::{build_commit, compiled_version, describe};
    format!(
        "reliaburger {}",
        describe(&compiled_version(), build_commit())
    )
}

/// A format cannot be safely admitted or opened.
///
/// Every message leads with what was found and what this binary needs,
/// because `journalctl` cuts a long line at the terminal's width. The
/// remedy and the policy link follow.
#[derive(Debug, thiserror::Error)]
pub enum CompatibilityError {
    /// The other side (a peer, a joining node, a candidate binary) speaks a
    /// different protocol or storage generation.
    #[error(
        "incompatible cluster formats: found protocol {}, state {}; this binary ({}) needs protocol {}, state {}. \
         Pre-1.0 builds don't migrate or mix formats: run the same reliaburger release on every node, \
         or recreate the cluster. See {POLICY_URL}",
        .found.protocol,
        .found.state,
        this_binary(),
        .expected.protocol,
        .expected.state
    )]
    Mismatch {
        /// The pair the other side advertised.
        found: Compatibility,
        /// The pair this binary requires.
        expected: Compatibility,
    },
    /// Existing state has no format stamp, so no binary vouches for it.
    #[error(
        "unversioned state at {}; this binary ({}) needs state format {} and won't adopt unstamped data. \
         Pre-1.0 builds don't migrate state: move the directory aside and recreate the cluster. See {POLICY_URL}",
        .0.display(),
        this_binary(),
        CURRENT.state
    )]
    DevelopmentState(std::path::PathBuf),
    /// The stamp was written by a binary with a different state format.
    #[error(
        "incompatible state format: found {found}; this binary ({}) needs {expected}. \
         Pre-1.0 builds don't migrate state: run the reliaburger release that wrote {}, \
         or move the data aside and recreate the cluster. See {POLICY_URL}",
        this_binary(),
        .stamp.display()
    )]
    StateMismatch {
        /// The stamp file that was read.
        stamp: std::path::PathBuf,
        /// The state format the stamp records.
        found: u32,
        /// The state format this binary writes.
        expected: u32,
    },
    /// The stamp exists but can't be read as a format number.
    #[error(
        "unreadable state format stamp at {}; this binary ({}) needs state format {}. \
         Pre-1.0 builds don't migrate state, and a hand-written stamp won't make the data compatible: \
         run the reliaburger release that wrote it, or move it aside and recreate the cluster. See {POLICY_URL}",
        .0.display(),
        this_binary(),
        CURRENT.state
    )]
    InvalidState(std::path::PathBuf),
    /// A filesystem operation failed without permission to reinterpret the data.
    #[error("state compatibility I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl Compatibility {
    /// Reject formats that do not explicitly match this binary's contract.
    pub fn require_current(self) -> Result<(), CompatibilityError> {
        if self == CURRENT {
            Ok(())
        } else {
            Err(CompatibilityError::Mismatch {
                found: self,
                expected: CURRENT,
            })
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateStamp {
    format: u32,
}

/// Validate durable state before opening any subsystem, or stamp a fresh directory.
///
/// A freshly enrolled identity may precede first boot. All other unmarked state
/// is refused without modification. This performs blocking filesystem I/O.
pub fn ensure_state_compatible(directory: &Path) -> Result<(), CompatibilityError> {
    std::fs::create_dir_all(directory)?;
    let stamp = directory.join(STATE_STAMP);
    match std::fs::symlink_metadata(&stamp) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > 4096 {
                return Err(CompatibilityError::InvalidState(stamp));
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&stamp)?
                .take(4097)
                .read_to_end(&mut bytes)?;
            let decoded = serde_json::from_slice::<StateStamp>(&bytes)
                .map_err(|_| CompatibilityError::InvalidState(stamp.clone()))?;
            if decoded.format != CURRENT.state {
                return Err(CompatibilityError::StateMismatch {
                    stamp,
                    found: decoded.format,
                    expected: CURRENT.state,
                });
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() != "identity" || !entry.file_type()?.is_dir() {
            return Err(CompatibilityError::DevelopmentState(
                directory.to_path_buf(),
            ));
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let bytes = serde_json::to_vec(&StateStamp {
        format: CURRENT.state,
    })
    .map_err(std::io::Error::other)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&stamp)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_gets_a_durable_stamp_that_allows_restart() {
        let directory = tempfile::tempdir().unwrap();
        ensure_state_compatible(directory.path()).unwrap();
        std::fs::write(directory.path().join("node-state"), b"keep").unwrap();
        ensure_state_compatible(directory.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(directory.path().join(STATE_STAMP))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn development_state_is_never_stamped_or_modified() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old-snapshot");
        std::fs::write(&old, b"keep").unwrap();
        assert!(matches!(
            ensure_state_compatible(directory.path()),
            Err(CompatibilityError::DevelopmentState(_))
        ));
        assert_eq!(std::fs::read(old).unwrap(), b"keep");
        assert!(!directory.path().join(STATE_STAMP).exists());
    }

    #[test]
    fn future_or_corrupt_stamps_are_preserved_and_refused() {
        for bytes in [
            b"broken".as_slice(),
            br#"{"format":999}"#,
            br#"{"format":1}"#,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let stamp = directory.path().join(STATE_STAMP);
            std::fs::write(&stamp, bytes).unwrap();
            assert!(ensure_state_compatible(directory.path()).is_err());
            assert_eq!(std::fs::read(stamp).unwrap(), bytes);
        }
    }

    #[test]
    fn enrolment_identity_can_precede_first_boot() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("identity")).unwrap();
        ensure_state_compatible(directory.path()).unwrap();
    }

    /// The generations before task arrays. There's no compatibility before
    /// 1.0.0, so nodes and data from then are refused, not migrated.
    const BEFORE_TASK_ARRAYS: Compatibility = Compatibility {
        protocol: 34,
        state: 49,
    };

    #[test]
    fn peers_and_state_from_before_task_arrays_are_refused() {
        assert!(BEFORE_TASK_ARRAYS.require_current().is_err());
        let directory = tempfile::tempdir().unwrap();
        let stamp = directory.path().join(STATE_STAMP);
        let old = format!(r#"{{"format":{}}}"#, BEFORE_TASK_ARRAYS.state);
        std::fs::write(&stamp, &old).unwrap();
        assert!(matches!(
            ensure_state_compatible(directory.path()),
            Err(CompatibilityError::StateMismatch { found, expected, .. })
                if found == BEFORE_TASK_ARRAYS.state && expected == CURRENT.state
        ));
        assert_eq!(std::fs::read_to_string(stamp).unwrap(), old);
    }

    #[test]
    fn either_format_mismatch_refuses_admission() {
        CURRENT.require_current().unwrap();
        assert!(
            Compatibility {
                protocol: CURRENT.protocol + 1,
                ..CURRENT
            }
            .require_current()
            .is_err()
        );
        assert!(
            Compatibility {
                state: CURRENT.state + 1,
                ..CURRENT
            }
            .require_current()
            .is_err()
        );
    }

    /// The first sentence, which has to survive a journal line cut at
    /// terminal width.
    fn lead(message: &str) -> &str {
        message.split(". ").next().unwrap()
    }

    #[test]
    fn protocol_refusal_leads_with_both_pairs_and_names_the_way_out() {
        let message = CompatibilityError::Mismatch {
            found: Compatibility {
                protocol: 26,
                state: 43,
            },
            expected: Compatibility {
                protocol: 27,
                state: 44,
            },
        }
        .to_string();
        assert!(
            lead(&message).starts_with(
                "incompatible cluster formats: found protocol 26, state 43; this binary (reliaburger "
            ),
            "{message}"
        );
        assert!(
            lead(&message).ends_with("needs protocol 27, state 44"),
            "{message}"
        );
        assert!(message.contains(env!("CARGO_PKG_VERSION")), "{message}");
        assert!(
            message.contains("Pre-1.0 builds don't migrate"),
            "{message}"
        );
        assert!(message.contains("recreate the cluster"), "{message}");
        assert!(message.ends_with(POLICY_URL), "{message}");
    }

    #[test]
    fn state_format_refusal_leads_with_found_and_expected() {
        let stamp = std::path::PathBuf::from("/var/lib/reliaburger/data/state-format.json");
        let message = CompatibilityError::StateMismatch {
            stamp: stamp.clone(),
            found: 43,
            expected: 44,
        }
        .to_string();
        assert!(
            lead(&message)
                .starts_with("incompatible state format: found 43; this binary (reliaburger "),
            "{message}"
        );
        assert!(lead(&message).ends_with("needs 44"), "{message}");
        assert!(
            message.contains("Pre-1.0 builds don't migrate state"),
            "{message}"
        );
        assert!(message.contains(&stamp.display().to_string()), "{message}");
        assert!(message.contains("recreate the cluster"), "{message}");
        assert!(message.ends_with(POLICY_URL), "{message}");
    }

    #[test]
    fn unreadable_and_unversioned_state_refusals_name_the_expected_format() {
        let expected = format!("needs state format {}", CURRENT.state);
        for message in [
            CompatibilityError::InvalidState("/data/state-format.json".into()).to_string(),
            CompatibilityError::DevelopmentState("/data".into()).to_string(),
        ] {
            assert!(lead(&message).contains(&expected), "{message}");
            assert!(
                message.contains("Pre-1.0 builds don't migrate state"),
                "{message}"
            );
            assert!(message.ends_with(POLICY_URL), "{message}");
        }
    }

    #[test]
    fn a_stamp_from_another_generation_reports_what_it_found() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(STATE_STAMP), br#"{"format":1}"#).unwrap();
        match ensure_state_compatible(directory.path()) {
            Err(CompatibilityError::StateMismatch {
                found, expected, ..
            }) => {
                assert_eq!(found, 1);
                assert_eq!(expected, CURRENT.state);
            }
            other => panic!("expected a state mismatch, got {other:?}"),
        }
    }

    #[test]
    fn require_current_reports_the_pair_it_was_given() {
        let found = Compatibility {
            protocol: CURRENT.protocol + 1,
            ..CURRENT
        };
        match found.require_current() {
            Err(CompatibilityError::Mismatch {
                found: reported,
                expected,
            }) => {
                assert_eq!(reported, found);
                assert_eq!(expected, CURRENT);
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }
}

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
/// and inode beside its offset, and the volume snapshot layout (reversible
/// volume slugs, per-destination export receipts, restore journals and
/// volume quotas in the sidecar).
pub const CURRENT: Compatibility = Compatibility {
    protocol: 27,
    state: 46,
};

/// Name of the durable format stamp at the root of a node's data directory.
pub const STATE_STAMP: &str = "state-format.json";

/// Where the compatibility policy every refusal points to lives.
pub const POLICY_URL: &str = "https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#compatibility-before-100";

/// This binary as a refusal names it: `reliaburger v0.1.1 (3fcb1fd)`, or
/// just the version when the build didn't know its commit.
fn this_binary() -> String {
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

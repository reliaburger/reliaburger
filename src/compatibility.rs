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
/// cluster upgrade and the 503 a node answers a directive with when the
/// binary's registry is unavailable (the orchestrator retries it).
pub const CURRENT: Compatibility = Compatibility {
    protocol: 27,
    state: 43,
};

/// Name of the durable format stamp at the root of a node's data directory.
pub const STATE_STAMP: &str = "state-format.json";

/// A format cannot be safely admitted or opened.
#[derive(Debug, thiserror::Error)]
pub enum CompatibilityError {
    /// The binary advertises a different protocol or storage generation.
    #[error("incompatible cluster formats: received {received:?}, required {required:?}")]
    Mismatch {
        received: Compatibility,
        required: Compatibility,
    },
    /// Existing development state has no supported format stamp.
    #[error("unversioned development state at {0}; create a fresh cluster for 0.1.0")]
    DevelopmentState(std::path::PathBuf),
    /// The stamp cannot establish a supported storage format.
    #[error(
        "invalid or incompatible state format at {0}; preserve the data and use a compatible binary"
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
                received: self,
                required: CURRENT,
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
                return Err(CompatibilityError::InvalidState(stamp));
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
}

//! Durable ownership and convergence state for the placement reconciler.
//!
//! Ownership is persisted before a deployment is queued. Pending entries survive
//! interrupted deployment, so a withdrawn assignment still has an owner to retire
//! after restart. Applied fingerprints only skip work whose runtime inventory was
//! verified. Corrupt or unreadable state refuses reconciliation.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Progress of an assignment whose resources the reconciler owns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub enum AssignmentState {
    /// Ownership is durable, but deployment has not been confirmed.
    Pending,
    /// The assignment completed successfully at this serialised specification.
    Applied { fingerprint: String },
}

/// Owned applications keyed by `(name, namespace)`, including interrupted work.
pub type AppliedMap = BTreeMap<(String, String), AssignmentState>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedCheckpoint {
    schema: u32,
    entries: Vec<AppliedEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedEntry {
    namespace: String,
    name: String,
    assignment: AssignmentState,
}

const SCHEMA: u32 = 2;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

/// The checkpoint file name within the reconciler's state directory.
pub const CHECKPOINT_FILE: &str = "applied-placements.json";

/// The full checkpoint path within a state directory.
pub fn checkpoint_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CHECKPOINT_FILE)
}

/// Load ownership, refusing invalid or unreadable state. Only a missing file is
/// empty. This performs blocking I/O; asynchronous callers use `spawn_blocking`.
pub fn load(path: &Path) -> std::io::Result<AppliedMap> {
    let Some(checkpoint) = crate::durable::read_json_if_exists::<AppliedCheckpoint>(
        path,
        MAX_CHECKPOINT_BYTES,
        crate::durable::Access::Regular,
    )?
    else {
        return Ok(AppliedMap::new());
    };
    if checkpoint.schema != SCHEMA {
        return Err(std::io::Error::other(
            "unsupported placement checkpoint schema",
        ));
    }
    let mut owned = AppliedMap::new();
    for entry in checkpoint.entries {
        if owned
            .insert((entry.name, entry.namespace), entry.assignment)
            .is_some()
        {
            return Err(std::io::Error::other(
                "duplicate placement checkpoint owner",
            ));
        }
    }
    Ok(owned)
}

/// Atomically persist private ownership and sync both the file and its directory
/// before acknowledging it. Errors retain the caller's previous in-memory state.
/// This performs blocking I/O; asynchronous callers use `spawn_blocking`.
pub fn save(path: &Path, applied: &AppliedMap) -> std::io::Result<()> {
    let checkpoint = AppliedCheckpoint {
        schema: SCHEMA,
        entries: applied
            .iter()
            .map(|((name, namespace), assignment)| AppliedEntry {
                namespace: namespace.clone(),
                name: name.clone(),
                assignment: assignment.clone(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&checkpoint)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(std::io::Error::other("placement checkpoint exceeds 64 MiB"));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> AppliedMap {
        BTreeMap::from([(("api".into(), "payments".into()), AssignmentState::Pending)])
    }

    #[test]
    fn missing_checkpoint_has_no_owners() {
        let root = tempfile::tempdir().unwrap();
        assert!(load(&checkpoint_path(root.path())).unwrap().is_empty());
    }

    #[test]
    fn pending_and_applied_ownership_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = checkpoint_path(root.path());
        let mut owned = pending();
        owned.insert(
            ("api".into(), "default".into()),
            AssignmentState::Applied {
                fingerprint: "spec".into(),
            },
        );
        save(&path, &owned).unwrap();
        assert_eq!(load(&path).unwrap(), owned);
    }

    #[test]
    fn invalid_checkpoint_never_becomes_an_empty_inventory() {
        let root = tempfile::tempdir().unwrap();
        let path = checkpoint_path(root.path());
        for bytes in [
            "not JSON",
            r#"{"schema":1,"entries":[]}"#,
            r#"{"schema":2,"entries":[{"namespace":"p","name":"a","assignment":{"state":"pending"}},{"namespace":"p","name":"a","assignment":{"state":"pending"}}]}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(load(&path).is_err(), "accepted {bytes}");
        }
    }

    #[test]
    fn checkpoint_replacement_is_private_and_preserves_the_previous_inode() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = checkpoint_path(root.path());
        save(&path, &pending()).unwrap();
        let mut previous = std::fs::File::open(&path).unwrap();
        save(&path, &AppliedMap::new()).unwrap();
        let mut bytes = Vec::new();
        previous.read_to_end(&mut bytes).unwrap();
        let original: AppliedCheckpoint = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(original.entries.len(), 1);
        assert!(load(&path).unwrap().is_empty());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn failed_checkpoint_replacement_preserves_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let path = checkpoint_path(root.path());
        std::fs::create_dir(&path).unwrap();
        let marker = path.join("retain-me");
        std::fs::write(&marker, b"owned").unwrap();
        assert!(save(&path, &pending()).is_err());
        assert_eq!(std::fs::read(marker).unwrap(), b"owned");
        assert!(load(&path).is_err());
    }

    #[test]
    fn symlink_checkpoint_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = checkpoint_path(root.path());
        let other = root.path().join("other.json");
        save(&other, &pending()).unwrap();
        std::os::unix::fs::symlink(other, &path).unwrap();
        assert!(load(&path).is_err());
    }
}

//! Private standalone storage for the same deterministic job execution state.

use crate::meat::task_array_store::{TaskArrayApplied, TaskArrayWrite, TaskArrays};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: crate::compatibility::Compatibility,
    next_id: u64,
    revision: u64,
    arrays: TaskArrays,
}

pub(super) struct JobStore {
    path: PathBuf,
    checkpoint: Checkpoint,
    fenced: bool,
}

impl JobStore {
    pub(super) fn open(directory: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        std::fs::File::open(directory.parent().unwrap_or(Path::new(".")))?.sync_all()?;
        let path = directory.join("jobs.json");
        let checkpoint = match std::fs::metadata(&path) {
            Ok(metadata) => {
                if metadata.len() > MAX_STORE_BYTES {
                    return Err(std::io::Error::other("job store exceeds 64 MiB"));
                }
                let checkpoint: Checkpoint = serde_json::from_slice(&std::fs::read(&path)?)
                    .map_err(std::io::Error::other)?;
                if checkpoint.version != crate::compatibility::CURRENT
                    || checkpoint.next_id == 0
                    || checkpoint
                        .arrays
                        .ids()
                        .iter()
                        .any(|id| *id >= checkpoint.next_id)
                {
                    return Err(std::io::Error::other(
                        "incompatible job storage or invalid identity counter",
                    ));
                }
                checkpoint
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let checkpoint = Checkpoint {
                    version: crate::compatibility::CURRENT,
                    next_id: 1,
                    revision: 0,
                    arrays: TaskArrays::default(),
                };
                crate::sesame::identity::atomic_write_mode(
                    &path,
                    &serde_json::to_vec(&checkpoint)?,
                    Some(0o600),
                )?;
                checkpoint
            }
            Err(error) => return Err(error),
        };
        reserve_progress(&checkpoint)?;
        Ok(Self {
            path,
            checkpoint,
            fenced: false,
        })
    }

    pub(super) fn ready(&self) -> bool {
        !self.fenced
    }
    pub(super) fn revision(&self) -> u64 {
        self.checkpoint.revision
    }

    pub(super) fn arrays(&self) -> &TaskArrays {
        &self.checkpoint.arrays
    }

    pub(super) fn apply(&mut self, write: &TaskArrayWrite) -> Result<Option<u64>, String> {
        if self.fenced {
            return Err(
                "standalone job storage is fenced after publication failure; restart and reconcile"
                    .into(),
            );
        }
        let mut next = self.checkpoint.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or("job control revision exhausted")?;
        let ids = u64::try_from(next.arrays.planned_ids(write).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        next.next_id
            .checked_add(ids)
            .ok_or("job identity counter exhausted")?;
        let result = next
            .arrays
            .apply(write, || {
                let id = next.next_id;
                next.next_id += 1;
                id
            })
            .map_err(|e| e.to_string())?;
        reserve_progress(&next).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err("job store exceeds 64 MiB".into());
        }
        if let Err(error) =
            crate::sesame::identity::atomic_write_mode(&self.path, &bytes, Some(0o600))
        {
            self.fenced = true;
            return Err(format!("job storage publication failed: {error}"));
        }
        self.checkpoint = next;
        Ok(match result {
            TaskArrayApplied::Registered { batch_id } => Some(batch_id),
            _ => None,
        })
    }
}

// A bounded initial snapshot can grow as unordered grants and results arrive.
// Reserve the worst sparse representation before admission, leaving publication
// failure fencing for actual I/O failures rather than predictable capacity loss.
fn reserve_progress(checkpoint: &Checkpoint) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(checkpoint)?.len() as u64 + 2 * 1024 * 1024;
    for (_, record) in checkpoint.arrays.active() {
        let actual = serde_json::to_vec(record)?.len() as u64;
        let reserved = 256 * 1024 + 512 * u64::from(record.state.spec.chunk_count());
        bytes = bytes.saturating_add(reserved.saturating_sub(actual));
    }
    if bytes > MAX_STORE_BYTES {
        return Err(std::io::Error::other(
            "job progress would exceed standalone storage capacity; wait for active runs to settle or increase chunk size",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::task_array::TaskArraySpec;
    use crate::meat::task_array_store::TaskArrayWrite;

    fn registration() -> TaskArrayWrite {
        TaskArrayWrite::Register {
            name: "test".into(),
            namespace: "default".into(),
            template: Box::new(toml::from_str("runtime='process'\nexec='/bin/true'").unwrap()),
            spec: TaskArraySpec::with_count(1),
            submitted_at_epoch_secs: 10,
        }
    }

    #[test]
    fn admission_reserves_future_sparse_progress_before_acknowledging_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JobStore::open(dir.path()).unwrap();
        let mut write = registration();
        if let TaskArrayWrite::Register { spec, .. } = &mut write {
            spec.count = 65_536;
            spec.chunk_size = 1;
        }
        assert_eq!(store.apply(&write).unwrap(), Some(1));
        assert!(
            store.apply(&write).is_err(),
            "future progress must fit before another run is accepted"
        );
        assert_eq!(store.checkpoint.next_id, 2);
        assert!(store.ready());
        let node = crate::meat::NodeId::new("worker");
        store
            .apply(&TaskArrayWrite::Sync {
                batch_id: 1,
                now_epoch_secs: 11,
                results: vec![],
                grants: vec![(
                    node,
                    crate::meat::index_set::IndexRangeSet::from_range(0..=100),
                )],
            })
            .unwrap();
        drop(store);
        assert!(
            JobStore::open(dir.path())
                .unwrap()
                .arrays()
                .get(1)
                .is_some()
        );
    }
    #[test]
    fn reopening_refuses_impossible_execution_partition_or_specification() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JobStore::open(dir.path()).unwrap();
        store.apply(&registration()).unwrap();
        let file = dir.path().join("jobs.json");
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        for (field, value) in [
            ("queued", serde_json::json!([])),
            ("spec", serde_json::json!({"count":0})),
        ] {
            let mut json = original.clone();
            json["arrays"]["arrays"]["1"]["state"][field] = value;
            std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
            assert!(
                JobStore::open(dir.path()).is_err(),
                "accepted invalid {field}"
            );
        }
    }

    #[test]
    fn a_restart_preserves_execution_and_the_next_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JobStore::open(dir.path()).unwrap();
        assert_eq!(store.apply(&registration()).unwrap(), Some(1));
        drop(store);
        let mut store = JobStore::open(dir.path()).unwrap();
        assert!(store.arrays().get(1).is_some());
        assert_eq!(store.apply(&registration()).unwrap(), Some(2));
    }

    #[test]
    fn publication_failure_fences_all_further_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = JobStore::open(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("jobs.json")).unwrap();
        std::fs::create_dir(dir.path().join("jobs.json")).unwrap();
        assert!(store.apply(&registration()).is_err());
        assert!(store.arrays().get(1).is_none());
        std::fs::remove_dir(dir.path().join("jobs.json")).unwrap();
        assert!(store.apply(&registration()).is_err());
    }

    #[test]
    fn reopening_refuses_corrupt_or_incompatible_storage() {
        let dir = tempfile::tempdir().unwrap();
        JobStore::open(dir.path()).unwrap();
        let file = dir.path().join("jobs.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        json["version"]["state"] = serde_json::json!(0);
        std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(JobStore::open(dir.path()).is_err());
        std::fs::write(file, b"broken").unwrap();
        assert!(JobStore::open(dir.path()).is_err());
    }
}

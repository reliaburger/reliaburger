//! Journalled replacement of the complete Raft directory during offline recovery.
//! A durable intent precedes either rename. Startup finishes an interrupted
//! install before opening (or creating) a log. The old directory is retained.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path};
use std::sync::Arc;

use super::{DesiredState, state_machine::CouncilStateMachine};

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Serialise storage opening with recovery, including the gap between renames.
/// Live stores keep their redb locks after the opening guard is released.
pub(crate) fn lock(raft: &Path) -> io::Result<File> {
    let parent = raft
        .parent()
        .ok_or_else(|| io_error("Raft directory needs a parent"))?;
    fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(raft.with_extension("recovery-lock"))?;
    file.try_lock().map_err(io_error)?;
    Ok(file)
}

/// Hold every existing store's redb lock, refusing a store a live node has
/// open. A store redb can't open for any other reason (a damaged file is
/// exactly what recovery exists for) doesn't block recovery: the whole
/// directory is moved aside and retained, never deleted.
fn lock_existing(raft: &Path) -> io::Result<Vec<redb::Database>> {
    let mut held = Vec::new();
    for name in ["log.redb", "snapshot.redb"] {
        let path = raft.join(name);
        if !path.exists() {
            continue;
        }
        match redb::Database::open(&path) {
            Ok(db) => held.push(db),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(io_error(format!(
                    "refusing to replace {}: a running node holds it open",
                    path.display()
                )));
            }
            Err(error) => eprintln!(
                "recovery: {} is unreadable ({error}); it is retained beside the replacement",
                path.display()
            ),
        }
    }
    Ok(held)
}

fn validate_replacement(path: &Path) -> io::Result<()> {
    let db = Arc::new(redb::Database::open(path.join("snapshot.redb")).map_err(io_error)?);
    if !CouncilStateMachine::snapshot_present(&db).map_err(io_error)? {
        return Err(io_error("recovery replacement contains no snapshot"));
    }
    CouncilStateMachine::with_store(db).map_err(io_error)?;
    Ok(())
}

/// Resume a durable recovery intent. The caller must hold `lock(raft)`.
pub(crate) fn finish_pending(raft: &Path) -> io::Result<()> {
    let journal = raft.with_extension("recovery");
    let name = match fs::read_to_string(&journal) {
        Ok(name) => name,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut components = Path::new(&name).components();
    if !name.starts_with(".raft-recovery-")
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(io_error(
            "invalid recovery journal; refusing to open Raft storage",
        ));
    }
    let parent = raft
        .parent()
        .ok_or_else(|| io_error("Raft directory needs a parent"))?;
    let transaction = parent.join(name);
    let replacement = transaction.join("replacement");
    let previous = transaction.join("previous");
    // Keep these locks through both renames; never unlink a live redb inode.
    let _old_stores = if replacement.exists() && raft.exists() {
        if previous.exists() {
            return Err(io_error(
                "ambiguous recovery transaction; previous state retained",
            ));
        }
        lock_existing(raft)?
    } else {
        Vec::new()
    };
    if replacement.exists() {
        validate_replacement(&replacement)?;
        if raft.exists() {
            fs::rename(raft, &previous)?;
            sync_dir(&transaction)?;
            sync_dir(parent)?;
        } else if !previous.exists() {
            return Err(io_error("recovery journal has no previous state"));
        }
        fs::rename(&replacement, raft)?;
        sync_dir(&transaction)?;
        sync_dir(parent)?;
    } else if !previous.exists() || !raft.exists() {
        return Err(io_error(
            "incomplete recovery journal; refusing to create empty state",
        ));
    }
    validate_replacement(raft)?;
    fs::remove_file(journal)?;
    sync_dir(parent)
}

/// Remove the Raft directory of a stopped node (`relish council re-enrol`),
/// under the same lock as recovery and startup. Refuses a store a running
/// node holds open, and finishes any interrupted recovery first so it can't
/// resurrect the removed state on the next start.
pub(crate) fn remove(raft: &Path) -> io::Result<()> {
    let _guard = lock(raft)?;
    finish_pending(raft)?;
    if !raft.exists() {
        return Ok(());
    }
    drop(lock_existing(raft)?);
    fs::remove_dir_all(raft)?;
    let parent = raft
        .parent()
        .ok_or_else(|| io_error("Raft directory needs a parent"))?;
    sync_dir(parent)
}

pub(crate) fn replace(raft: &Path, state: DesiredState) -> io::Result<()> {
    let _guard = lock(raft)?;
    finish_pending(raft)?;
    fs::create_dir_all(raft)?;
    let stores = lock_existing(raft)?;
    let parent = raft
        .parent()
        .ok_or_else(|| io_error("Raft directory needs a parent"))?;
    let stage = tempfile::Builder::new()
        .prefix(".raft-recovery-")
        .tempdir_in(parent)?;
    let replacement = stage.path().join("replacement");
    fs::create_dir(&replacement)?;
    let db = redb::Database::create(replacement.join("snapshot.redb")).map_err(io_error)?;
    CouncilStateMachine::persist_recovered_snapshot(&db, state).map_err(io_error)?;
    drop(db);
    validate_replacement(&replacement)?;
    sync_dir(&replacement)?;
    sync_dir(stage.path())?;
    // From here even an error must retain both stores for restart/retry.
    let transaction = stage.keep();
    sync_dir(parent)?;
    let name = transaction
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io_error("invalid transaction name"))?;
    crate::sesame::identity::atomic_write(&raft.with_extension("recovery"), name.as_bytes())?;
    sync_dir(parent)?;
    // finish_pending reacquires redb locks while the directory guard remains.
    drop(stores);
    finish_pending(raft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audit_restart_finishes_each_recovery_rename_boundary() {
        for boundary in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let raft = root.path().join("raft");
            replace(&raft, DesiredState::default()).unwrap();
            fs::write(raft.join("original-evidence"), b"keep me").unwrap();
            let transaction = root.path().join(".raft-recovery-interrupted");
            let replacement = transaction.join("replacement");
            fs::create_dir_all(&replacement).unwrap();
            let db = redb::Database::create(replacement.join("snapshot.redb")).unwrap();
            let mut state = DesiredState::default();
            state.config.insert("restored".into(), "yes".into());
            CouncilStateMachine::persist_recovered_snapshot(&db, state).unwrap();
            drop(db);
            fs::write(
                raft.with_extension("recovery"),
                b".raft-recovery-interrupted",
            )
            .unwrap();
            if boundary >= 1 {
                fs::rename(&raft, transaction.join("previous")).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&replacement, &raft).unwrap();
            }
            let (log, fresh, machine) = crate::cluster::runtime::open_raft_storage(&raft, None)
                .await
                .unwrap();
            assert!(fresh);
            assert_eq!(
                machine
                    .desired_state()
                    .await
                    .config
                    .get("restored")
                    .map(String::as_str),
                Some("yes")
            );
            assert_eq!(
                fs::read(transaction.join("previous/original-evidence")).unwrap(),
                b"keep me"
            );
            assert!(!raft.with_extension("recovery").exists());
            drop(log);
            drop(machine);
            crate::cluster::runtime::open_raft_storage(&raft, None)
                .await
                .unwrap();
        }
    }
}

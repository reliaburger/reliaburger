//! Garbage collection only for command IDs that are never reused.

use super::*;

impl ProcessControl {
    /// Remove positive terminal command evidence under exclusive collection ownership.
    /// Only OwnedCommands may use this: ordinary workload instance IDs are reusable.
    pub(crate) async fn prune_retired_commands(&self) -> io::Result<usize> {
        let launches = self.inventory().await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            match std::fs::symlink_metadata(&this.root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(error),
                Ok(_) => validate_directory(&this.root)?,
            }
            let garbage = this
                .root
                .parent()
                .ok_or_else(|| io::Error::other("command collection has no parent"))?
                .join("retired-commands");
            create_directory(&garbage)?;
            reap_garbage(&garbage)?;
            let mut removed = 0;
            for launch in launches {
                let id = launch.instance_id;
                let observed = this.load(&id)?;
                if !terminal(&observed.phase) {
                    continue;
                }
                let directory = this.directory(&id)?;
                let _operation = operation_lock(&directory)?;
                let _owner = wait_for_owner_lock(&directory)?;
                let current = this.load(&id)?;
                if current.nonce != observed.nonce || !terminal(&current.phase) {
                    return Err(io::Error::other(
                        "command generation changed before pruning",
                    ));
                }
                let name = format!("{}-{}", id.0, current.nonce);
                validate_garbage_name(&name)?;
                let destination = garbage.join(name);
                match std::fs::symlink_metadata(&destination) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                    Ok(_) => return Err(io::Error::other("command garbage already exists")),
                }
                // A delayed start either holds these locks first and refuses the
                // terminal record, or reloads its now-absent original path later.
                // Atomic removal prevents partial deletion in the active inventory.
                std::fs::rename(&directory, &destination)?;
                File::open(&this.root)?.sync_all()?;
                File::open(&garbage)?.sync_all()?;
                std::fs::remove_dir_all(destination)?;
                File::open(&garbage)?.sync_all()?;
                removed += 1;
            }
            Ok(removed)
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl ProcessControl {
    /// Remove the retired generation of a reusable instance identity, so
    /// identities that never run again leave nothing behind (#678).
    ///
    /// The caller must already exclude other lifecycle operations on `id`, as
    /// an executor pool does for a slot it is retiring. A later `prepare`
    /// publishes a fresh generation, exactly as on a new node. A generation
    /// that may still own a process is refused and kept.
    pub(crate) async fn forget_retired(&self, id: &InstanceId) -> io::Result<()> {
        self.run(id, |this, id| {
            let directory = this.directory(&id)?;
            let garbage = this
                .root
                .parent()
                .ok_or_else(|| io::Error::other("process owners have no parent"))?
                .join("forgotten-process-owners");
            match std::fs::symlink_metadata(&garbage) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => reap_forgotten(&garbage)?,
            }
            match std::fs::symlink_metadata(&directory) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) => validate_directory(&directory)?,
            }
            let _operation = operation_lock(&directory)?;
            let _owner = wait_for_owner_lock(&directory)?;
            let record = this.load(&id)?;
            if !terminal(&record.phase) {
                return Err(io::Error::other("process generation has not retired"));
            }
            create_directory(&garbage)?;
            let destination = garbage.join(format!("{}-{}", id.0, record.nonce));
            // Atomic removal from the inventory first: an interrupted delete
            // leaves a private garbage directory, never a partial record.
            std::fs::rename(&directory, &destination)?;
            File::open(&this.root)?.sync_all()?;
            File::open(&garbage)?.sync_all()?;
            std::fs::remove_dir_all(destination)?;
            File::open(&garbage)?.sync_all()
        })
        .await
    }
}

/// Finish deletions an earlier [`ProcessControl::forget_retired`] began.
fn reap_forgotten(directory: &Path) -> io::Result<()> {
    validate_directory(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        validate_directory(&entry.path())?;
        // Publication into this private directory already removed the
        // generation from the inventory; only its bytes remain.
        std::fs::remove_dir_all(entry.path())?;
    }
    File::open(directory)?.sync_all()
}

fn terminal(phase: &OwnerPhase) -> bool {
    matches!(phase, OwnerPhase::Cancelled | OwnerPhase::Retired { .. })
}

fn validate_garbage_name(name: &str) -> io::Result<()> {
    let valid = name
        .strip_prefix("command-")
        .and_then(|value| value.split_once('-'))
        .is_some_and(|(id, nonce)| {
            [id, nonce].iter().all(|value| {
                value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        });
    if !valid {
        return Err(io::Error::other("invalid retired command identity"));
    }
    Ok(())
}

fn reap_garbage(directory: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("non-UTF-8 retired command identity"))?;
        validate_garbage_name(&name)?;
        validate_directory(&entry.path())?;
        // Atomic publication into this private directory proves retirement.
        // Contents may already be partly removed by an interrupted earlier pass.
        std::fs::remove_dir_all(entry.path())?;
    }
    File::open(directory)?.sync_all()
}

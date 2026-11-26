//! Startup tidying of what a crash or power loss left under the owners.

use super::*;

/// How old an unpublished `.preparing-*` directory must be before startup
/// removes it. A preparation publishes within milliseconds and holds its
/// operation lock throughout, so this only guards the instant between
/// creating the directory and locking it.
const STALE_PREPARATION: Duration = Duration::from_secs(10 * 60);

impl ProcessControl {
    /// Remove what a crash or power loss left behind (#2363): stale
    /// `.preparing-*` directories that were never published, and in each
    /// retired or cancelled launch's directory, its abandoned atomic-write
    /// temporaries and the `exec-*` directories no helper still uses.
    ///
    /// A launch that is prepared, running or retiring is left alone, and so
    /// is one whose owner or operation lock is busy: its retirement tidies
    /// it. Problems with one entry are logged and the rest still go.
    pub(crate) async fn remove_leftovers(&self) -> io::Result<()> {
        let this = self.clone();
        let in_flight = InFlight::enter(&self.in_flight);
        tokio::task::spawn_blocking(move || {
            let _in_flight = in_flight;
            match std::fs::symlink_metadata(&this.root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) => validate_directory(&this.root)?,
            }
            for entry in std::fs::read_dir(&this.root)? {
                let path = entry?.path();
                if let Err(error) = this.tidy_entry(&path) {
                    eprintln!("bun: cannot tidy {}: {error}", path.display());
                }
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    fn tidy_entry(&self, path: &Path) -> io::Result<()> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(());
        };
        if name.starts_with(".preparing-") {
            return remove_stale_preparation(path);
        }
        // Only a name `prepare` could have published is a launch.
        let Ok(directory) = self.directory(&InstanceId(name.to_owned())) else {
            return Ok(());
        };
        validate_directory(&directory)?;
        // Read first, so a prepared launch's owner never finds its lock
        // taken by this sweep.
        if !finished(&process_owner::load(&directory)?) {
            return Ok(());
        }
        let _operation = match operation_lock(&directory) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        let _owner = match process_owner::lock_owner(&directory) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        // Under both locks nothing can prepare a successor or write this
        // record, so what it says now holds while the sweep runs.
        if !finished(&process_owner::load(&directory)?) {
            return Ok(());
        }
        process_owner::remove_leftovers(&directory)
    }
}

fn finished(record: &OwnerRecord) -> bool {
    matches!(
        record.phase,
        OwnerPhase::Retired { .. } | OwnerPhase::Cancelled
    )
}

/// Remove an unpublished staging directory that no preparation still holds.
///
/// Staging never granted permission to run anything: publication renames
/// it, so a staging name that remains was abandoned by a crash.
fn remove_stale_preparation(path: &Path) -> io::Result<()> {
    validate_directory(path)?;
    let age = std::fs::symlink_metadata(path)?
        .modified()?
        .elapsed()
        .unwrap_or_default();
    if age < STALE_PREPARATION {
        return Ok(());
    }
    let _operation = match operation_lock(path) {
        Ok(lock) => lock,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::NotFound
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn private_directory(path: &Path) {
        std::fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }

    /// Prepare `id`, set its phase, and leave beside its record what a
    /// crash can: a helper directory that never got its record, and the
    /// temporary of an atomic write that never got its rename.
    async fn launch_with_leftovers(
        control: &ProcessControl,
        id: &InstanceId,
        phase: OwnerPhase,
    ) -> PathBuf {
        control
            .prepare(id, &super::super::tests::spec())
            .await
            .unwrap();
        let directory = control.directory(id).unwrap();
        let mut record = control.load(id).unwrap();
        record.phase = phase;
        process_owner::persist(&directory, &record).unwrap();
        private_directory(&directory.join("exec-crashed"));
        std::fs::write(directory.join(".reliaburger-crashed"), b"{").unwrap();
        directory
    }

    fn leftovers(directory: &Path) -> Vec<String> {
        entries(directory)
            .into_iter()
            .filter(|name| name.starts_with("exec-") || name.starts_with(".reliaburger-"))
            .collect()
    }

    #[tokio::test]
    async fn startup_removes_the_leftovers_of_finished_launches() {
        let root = tempfile::tempdir().unwrap();
        let control = ProcessControl::new(root.path().join("owners"), "/unused".into());
        let retired = launch_with_leftovers(
            &control,
            &InstanceId("default__retired-0".into()),
            OwnerPhase::Retired { exit_code: Some(0) },
        )
        .await;
        let cancelled = launch_with_leftovers(
            &control,
            &InstanceId("default__cancelled-0".into()),
            OwnerPhase::Cancelled,
        )
        .await;

        control.remove_leftovers().await.unwrap();

        for directory in [retired, cancelled] {
            assert!(leftovers(&directory).is_empty(), "{directory:?}");
            assert!(directory.join("owner.json").exists());
        }
        assert_eq!(control.inventory().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn startup_leaves_unfinished_and_live_launches_alone() {
        let root = tempfile::tempdir().unwrap();
        let control = ProcessControl::new(root.path().join("owners"), "/unused".into());
        let prepared = launch_with_leftovers(
            &control,
            &InstanceId("default__prepared-0".into()),
            OwnerPhase::Prepared,
        )
        .await;
        let running = launch_with_leftovers(
            &control,
            &InstanceId("default__running-0".into()),
            OwnerPhase::Running { pid: 1 },
        )
        .await;
        // A live owner holds its lock for its whole life; this one also
        // holds it over a record that already says Retired, the instant
        // between the owner's write and its exit.
        let busy = launch_with_leftovers(
            &control,
            &InstanceId("default__busy-0".into()),
            OwnerPhase::Retired { exit_code: Some(0) },
        )
        .await;
        let _live_owner = process_owner::lock_owner(&busy).unwrap();

        control.remove_leftovers().await.unwrap();

        for directory in [prepared, running, busy] {
            assert_eq!(
                leftovers(&directory),
                [".reliaburger-crashed", "exec-crashed"],
                "{directory:?}"
            );
        }
    }

    #[tokio::test]
    async fn startup_removes_only_stale_unpublished_preparations() {
        let root = tempfile::tempdir().unwrap();
        let owners = root.path().join("owners");
        private_directory(&owners);
        let stale = owners.join(".preparing-crashed");
        let fresh = owners.join(".preparing-in-progress");
        for directory in [&stale, &fresh] {
            private_directory(directory);
            std::fs::write(directory.join("owner.json"), b"{}").unwrap();
        }
        let long_ago = SystemTime::now() - STALE_PREPARATION - Duration::from_secs(60);
        File::open(&stale).unwrap().set_modified(long_ago).unwrap();
        let control = ProcessControl::new(owners.clone(), "/unused".into());

        control.remove_leftovers().await.unwrap();

        assert_eq!(entries(&owners), [".preparing-in-progress"]);
    }

    #[tokio::test]
    async fn a_node_without_owners_has_nothing_to_tidy() {
        let root = tempfile::tempdir().unwrap();
        let control = ProcessControl::new(root.path().join("owners"), "/unused".into());
        control.remove_leftovers().await.unwrap();
    }
}

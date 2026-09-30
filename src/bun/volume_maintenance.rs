//! Who owns an app's volumes while a snapshot operation runs.
//!
//! The agent loop checks for running instances before it accepts a
//! restore, then hands the work to a blocking task and moves on. Without a
//! reservation, a deploy (or a restart, or a second restore) accepted a
//! moment later could start using the volume while the restore renamed it
//! away. The loop records a reservation here before it dispatches the work;
//! the task carries a [`VolumeLease`], and the reservation lasts exactly as
//! long as the lease does. Dropping the lease, whether the task finished,
//! failed or panicked, releases it. The caller that asked for the restore
//! giving up doesn't: the blocking task still owns the lease. A task drops
//! its lease before it answers, not after, so a caller that fires its next
//! snapshot request the moment it has the answer never finds the volumes
//! still held by the operation it just watched finish.
//!
//! The map belongs to the agent loop alone, so it needs no lock. A lease
//! is an `Arc<()>` and the map keeps a `Weak` to it: the reservation is
//! live while `Weak::strong_count` is non-zero.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// What a reservation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeOperation {
    /// Replacing a live volume: nothing may start the app meanwhile.
    Restore,
    /// Taking or deleting a snapshot: excludes restores, not workloads.
    Snapshot,
}

/// Ownership of an app's volumes, held by the task doing the work.
#[derive(Debug)]
pub struct VolumeLease {
    _token: Arc<()>,
}

/// Live reservations, keyed by `(namespace, app)`.
#[derive(Debug, Default)]
pub struct VolumeMaintenance {
    held: HashMap<(String, String), (VolumeOperation, Weak<()>)>,
}

impl VolumeMaintenance {
    /// Reserve an app's volumes for `operation`, or `None` if another
    /// operation already holds them.
    pub fn reserve(
        &mut self,
        namespace: &str,
        app: &str,
        operation: VolumeOperation,
    ) -> Option<VolumeLease> {
        self.held.retain(|_, (_, token)| token.strong_count() > 0);
        let key = (namespace.to_string(), app.to_string());
        if self.held.contains_key(&key) {
            return None;
        }
        let token = Arc::new(());
        self.held.insert(key, (operation, Arc::downgrade(&token)));
        Some(VolumeLease { _token: token })
    }

    /// The operation currently holding an app's volumes, if any.
    pub fn current(&self, namespace: &str, app: &str) -> Option<VolumeOperation> {
        self.held
            .get(&(namespace.to_string(), app.to_string()))
            .filter(|(_, token)| token.strong_count() > 0)
            .map(|(operation, _)| *operation)
    }

    /// Whether a restore owns the app's volumes, so nothing may start it.
    pub fn restoring(&self, namespace: &str, app: &str) -> bool {
        self.current(namespace, app) == Some(VolumeOperation::Restore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_reservation_is_refused_until_the_lease_drops() {
        let mut maintenance = VolumeMaintenance::default();
        let lease = maintenance
            .reserve("default", "db", VolumeOperation::Restore)
            .unwrap();
        assert!(maintenance.restoring("default", "db"));
        assert!(
            maintenance
                .reserve("default", "db", VolumeOperation::Snapshot)
                .is_none()
        );
        // Other apps are independent.
        assert!(
            maintenance
                .reserve("default", "web", VolumeOperation::Restore)
                .is_some()
        );

        drop(lease);
        assert_eq!(maintenance.current("default", "db"), None);
        assert!(
            maintenance
                .reserve("default", "db", VolumeOperation::Snapshot)
                .is_some()
        );
    }

    #[test]
    fn a_snapshot_reservation_does_not_count_as_restoring() {
        let mut maintenance = VolumeMaintenance::default();
        let _lease = maintenance
            .reserve("default", "db", VolumeOperation::Snapshot)
            .unwrap();
        assert!(!maintenance.restoring("default", "db"));
        assert_eq!(
            maintenance.current("default", "db"),
            Some(VolumeOperation::Snapshot)
        );
    }

    #[test]
    fn a_lease_moved_into_another_thread_holds_until_that_thread_finishes() {
        let mut maintenance = VolumeMaintenance::default();
        let lease = maintenance
            .reserve("default", "db", VolumeOperation::Restore)
            .unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _lease = lease;
            release_rx.recv().unwrap();
        });
        assert!(maintenance.restoring("default", "db"));
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(!maintenance.restoring("default", "db"));
    }
}

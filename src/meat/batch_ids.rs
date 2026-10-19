//! The identity counter task arrays, job runs and manifests share.
//!
//! Every registration takes the next id from one monotonic counter, so
//! `relish batch status ID` names exactly one thing. Clustered, the counter
//! lives in the Raft `DesiredState`, so a restarted or new leader never
//! reuses an id. Standalone, the node keeps it beside its local arrays.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The next id to hand out. Monotonic; never reset or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchIds {
    /// Next id to allocate. Ids start at 1.
    pub next_batch_id: u64,
}

impl Default for BatchIds {
    fn default() -> Self {
        Self { next_batch_id: 1 }
    }
}

impl BatchIds {
    /// Check there is room for `count` more ids without taking any, so a
    /// registration that needs several either gets them all or none.
    pub fn preflight(&self, count: usize) -> Result<(), String> {
        if count == 0 {
            return Ok(());
        }
        let count = u64::try_from(count).map_err(|_| "batch id exhausted")?;
        self.next_batch_id
            .max(1)
            .checked_add(count)
            .ok_or("batch id exhausted")?;
        Ok(())
    }

    /// Take the next id. Callers run [`BatchIds::preflight`] first; an
    /// exhausted counter keeps returning its last id rather than wrapping
    /// to one already in use.
    pub fn allocate(&mut self) -> u64 {
        let id = self.next_batch_id.max(1);
        self.next_batch_id = id.saturating_add(1);
        id
    }
}

/// Seconds since the Unix epoch, saturating at zero on a pre-1970 clock.
pub fn epoch_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_at_one_and_never_repeat() {
        let mut ids = BatchIds::default();
        assert_eq!(ids.allocate(), 1);
        assert_eq!(ids.allocate(), 2);
        assert_eq!(ids.next_batch_id, 3);
    }

    #[test]
    fn preflight_refuses_a_registration_the_counter_cannot_cover() {
        let ids = BatchIds {
            next_batch_id: u64::MAX - 1,
        };
        assert!(ids.preflight(1).is_ok());
        assert!(ids.preflight(2).is_err());
        assert!(ids.preflight(0).is_ok());
    }
}

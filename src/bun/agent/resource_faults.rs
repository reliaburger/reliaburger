//! CPU, memory and disk I/O faults, written to their targets' cgroups off
//! the agent loop.
//!
//! A memory-pressure fault lowers a workload's `memory.high`, and the
//! kernel reclaims the workload's memory down to the new limit before the
//! write returns. On a workload with a lot of memory to give back that
//! takes as long as it takes. So, as with the signal faults, the loop picks
//! the targets' cgroups (a walk over its own maps), a task makes the writes
//! on a blocking thread, and the result comes back as a [`FollowUp`]. Only
//! then is the caller told the fault is in, and its saved values recorded
//! so clearing or expiring it can undo it.

use std::path::PathBuf;

use tokio::sync::oneshot;

use super::follow_ups::FollowUp;
use super::{BunAgent, BunError, Grill, InstanceId};
use crate::smoker::types::{FaultId, FaultReversal, FaultRule, FaultSummary, FaultType};

/// A fault that writes a limit into each target's cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CgroupFault {
    /// Cap `cpu.max` so the workload keeps `100 - percentage` of its cores.
    Cpu { percentage: u8, cores: Option<u32> },
    /// Lower `memory.high` toward the hard limit, which forces reclaim.
    Memory { percentage: u8 },
    /// Throttle `io.max` on one block device.
    DiskIo {
        bytes_per_sec: u64,
        write_only: bool,
        device: String,
    },
}

/// One target cgroup a fault wrote, with what clearing it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Written {
    pub(super) instance: String,
    pub(super) cgroup: PathBuf,
    pub(super) saved: String,
}

impl CgroupFault {
    /// The cgroup fault a fault type is, if it is one. `device` names the
    /// block device an I/O throttle keys on; only that fault calls it.
    pub(super) fn of(fault: &FaultType, device: impl FnOnce() -> String) -> Option<Self> {
        match fault {
            FaultType::CpuStress { percentage, cores } => Some(Self::Cpu {
                percentage: *percentage,
                cores: *cores,
            }),
            FaultType::MemoryPressure { percentage } => Some(Self::Memory {
                percentage: *percentage,
            }),
            FaultType::DiskIoThrottle {
                bytes_per_sec,
                write_only,
            } => Some(Self::DiskIo {
                bytes_per_sec: *bytes_per_sec,
                write_only: *write_only,
                device: device(),
            }),
            _ => None,
        }
    }

    /// Write the fault into one cgroup, returning what undoing it needs.
    fn apply(&self, cgroup: &std::path::Path) -> Result<String, String> {
        use crate::smoker::resource;
        match self {
            Self::Cpu { percentage, cores } => {
                let saved = resource::read_cpu_max(cgroup).map_err(|e| e.to_string())?;
                // O17: `cores` used to be parsed and thrown away while the
                // quota maths assumed one core, so on a 4-core node "80%
                // stress" actually took 95%.
                resource::apply_cpu_stress(cgroup, *percentage, *cores)
                    .map_err(|e| e.to_string())?;
                Ok(saved)
            }
            Self::Memory { percentage } => {
                let saved = resource::read_memory_high(cgroup).map_err(|e| e.to_string())?;
                resource::apply_memory_pressure(cgroup, *percentage).map_err(|e| e.to_string())?;
                Ok(saved)
            }
            Self::DiskIo {
                bytes_per_sec,
                write_only,
                device,
            } => {
                resource::apply_disk_io_throttle(cgroup, *bytes_per_sec, *write_only, device)
                    .map_err(|e| e.to_string())?;
                Ok(cgroup.to_string_lossy().into_owned())
            }
        }
    }

    /// Undo [`Self::apply`] on one cgroup. Best-effort: a failure is logged.
    pub(super) fn restore(&self, cgroup: &std::path::Path, saved: &str) {
        use crate::smoker::resource;
        let (file, result) = match self {
            Self::Cpu { .. } => ("cpu.max", resource::restore_cpu_max(cgroup, saved)),
            Self::Memory { .. } => ("memory.high", resource::restore_memory_high(cgroup, saved)),
            Self::DiskIo { device, .. } => {
                ("io.max", resource::remove_disk_io_throttle(cgroup, device))
            }
        };
        if let Err(e) = result {
            eprintln!(
                "smoker: rollback {file} on {} failed: {e}",
                cgroup.display()
            );
        }
    }

    /// What the registry keeps so clearing or expiring the fault undoes it.
    pub(super) fn reversal(&self, written: Vec<Written>) -> FaultReversal {
        match self {
            Self::Cpu { .. } => {
                FaultReversal::CpuMax(written.into_iter().map(|w| (w.instance, w.saved)).collect())
            }
            Self::Memory { .. } => FaultReversal::MemoryHigh(
                written.into_iter().map(|w| (w.instance, w.saved)).collect(),
            ),
            Self::DiskIo { device, .. } => FaultReversal::DiskIo {
                instances: written
                    .into_iter()
                    .map(|w| (w.saved, device.clone()))
                    .collect(),
            },
        }
    }

    /// Write the fault into every target's cgroup on a blocking thread.
    pub(super) async fn write(
        &self,
        targets: Vec<(InstanceId, PathBuf)>,
    ) -> Result<Vec<Written>, String> {
        let apply = self.clone();
        let restore = self.clone();
        write_cgroups(
            targets,
            move |cgroup| apply.apply(cgroup),
            move |cgroup, saved| restore.restore(cgroup, saved),
        )
        .await
    }
}

/// Run `apply` on each target cgroup, on a blocking thread.
///
/// If one fails partway through, the targets already written are rolled
/// back with `restore`, newest first, before the error is returned (M1):
/// without that, an earlier replica stayed throttled while the caller,
/// seeing the error, dropped the registry entry that would have let a later
/// clear undo it.
pub(super) async fn write_cgroups<A, R>(
    targets: Vec<(InstanceId, PathBuf)>,
    apply: A,
    restore: R,
) -> Result<Vec<Written>, String>
where
    A: Fn(&std::path::Path) -> Result<String, String> + Send + 'static,
    R: Fn(&std::path::Path, &str) + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut written: Vec<Written> = Vec::with_capacity(targets.len());
        for (id, cgroup) in targets {
            match apply(&cgroup) {
                Ok(saved) => written.push(Written {
                    instance: id.0,
                    cgroup,
                    saved,
                }),
                Err(e) => {
                    for done in written.iter().rev() {
                        restore(&done.cgroup, &done.saved);
                    }
                    return Err(e);
                }
            }
        }
        Ok(written)
    })
    .await
    .map_err(|e| format!("the cgroup writes failed: {e}"))?
}

/// A cgroup fault's task result, for the loop to finish.
pub(super) struct CgroupFaultWritten {
    rule_id: FaultId,
    fault: CgroupFault,
    result: Result<Vec<Written>, String>,
    response: oneshot::Sender<Result<FaultSummary, BunError>>,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Write a cgroup fault from a task, and answer `response` once it's in.
    pub(super) fn spawn_cgroup_fault(
        &mut self,
        rule: &FaultRule,
        fault: CgroupFault,
        response: oneshot::Sender<Result<FaultSummary, BunError>>,
    ) {
        let targets = match self.cgroup_fault_targets(rule) {
            Ok(targets) => targets,
            Err(reason) => {
                self.fault_registry.remove(rule.id);
                let _ = response.send(Err(BunError::FaultRejected { reason }));
                return;
            }
        };
        let rule_id = rule.id;
        self.follow_ups.spawn(async move {
            let result = fault.write(targets).await;
            FollowUp::CgroupFaultWritten(CgroupFaultWritten {
                rule_id,
                fault,
                result,
                response,
            })
        });
    }

    /// Record what a cgroup fault wrote and answer its caller.
    pub(super) fn finish_cgroup_fault(&mut self, done: CgroupFaultWritten) {
        let CgroupFaultWritten {
            rule_id,
            fault,
            result,
            response,
        } = done;
        let answer = match (result, self.fault_registry.get(rule_id).cloned()) {
            (Err(reason), _) => {
                self.fault_registry.remove(rule_id);
                Err(BunError::FaultRejected { reason })
            }
            (Ok(written), Some(rule)) => {
                self.record_reversal(rule_id, fault.reversal(written));
                Ok(FaultSummary::from(&rule))
            }
            // Cleared or expired while the writes were on their way, so no
            // reversal will undo them: undo them here. Putting a limit back
            // up doesn't make the kernel reclaim anything.
            (Ok(written), None) => {
                for done in written.iter().rev() {
                    fault.restore(&done.cgroup, &done.saved);
                }
                Err(BunError::FaultRejected {
                    reason: "the fault was cleared while it was being applied".into(),
                })
            }
        };
        let _ = response.send(answer);
    }

    /// The cgroups a cgroup fault writes, or why it can't write any.
    #[cfg(target_os = "linux")]
    pub(super) fn cgroup_fault_targets(
        &self,
        rule: &FaultRule,
    ) -> Result<Vec<(InstanceId, PathBuf)>, String> {
        let targets = self.target_instance_cgroups(rule);
        if targets.is_empty() {
            return Err(format!("no running instances of {}", rule.target_service));
        }
        Ok(targets)
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn cgroup_fault_targets(
        &self,
        rule: &FaultRule,
    ) -> Result<Vec<(InstanceId, PathBuf)>, String> {
        Err(format!("{} requires Linux cgroups", rule.fault_type))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(dir: &std::path::Path, count: usize) -> Vec<(InstanceId, PathBuf)> {
        (0..count)
            .map(|i| {
                let cgroup = dir.join(format!("web-{i}"));
                std::fs::create_dir(&cgroup).unwrap();
                std::fs::write(cgroup.join("memory.high"), "max").unwrap();
                (InstanceId(format!("default__web-{i}")), cgroup)
            })
            .collect()
    }

    fn squeeze(cgroup: &std::path::Path) -> Result<String, String> {
        let file = cgroup.join("memory.high");
        let saved = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        std::fs::write(&file, "1048576").map_err(|e| e.to_string())?;
        Ok(saved)
    }

    fn put_back(cgroup: &std::path::Path, saved: &str) {
        std::fs::write(cgroup.join("memory.high"), saved).unwrap();
    }

    #[test]
    fn cgroup_writes_run_off_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let targets = targets(dir.path(), 3);

        let written = crate::off_runtime::assert_runs_on_blocking_pool(write_cgroups(
            targets, squeeze, put_back,
        ))
        .unwrap();
        assert_eq!(written.len(), 3);
        for done in &written {
            assert_eq!(done.saved, "max");
            assert_eq!(
                std::fs::read_to_string(done.cgroup.join("memory.high")).unwrap(),
                "1048576"
            );
        }
        assert_eq!(written[0].instance, "default__web-0");
    }

    #[tokio::test]
    async fn a_failed_cgroup_write_rolls_back_the_ones_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = targets(dir.path(), 2);
        // A target whose cgroup has gone: reading its memory.high fails.
        targets.push((InstanceId("default__web-2".into()), dir.path().join("gone")));

        let error = write_cgroups(targets, squeeze, put_back).await.unwrap_err();

        assert!(!error.is_empty());
        for i in 0..2 {
            let file = dir.path().join(format!("web-{i}")).join("memory.high");
            assert_eq!(std::fs::read_to_string(file).unwrap(), "max");
        }
    }

    #[test]
    fn memory_pressure_is_a_cgroup_fault_that_keeps_its_saved_values() {
        let fault = CgroupFault::of(&FaultType::MemoryPressure { percentage: 50 }, || {
            unreachable!("only an I/O throttle names a device")
        })
        .unwrap();
        let reversal = fault.reversal(vec![Written {
            instance: "default__web-0".into(),
            cgroup: PathBuf::from("/sys/fs/cgroup/web"),
            saved: "max".into(),
        }]);
        assert!(matches!(
            reversal,
            FaultReversal::MemoryHigh(saved) if saved == vec![("default__web-0".to_string(), "max".to_string())]
        ));
    }

    #[test]
    fn an_io_throttle_reverses_by_cgroup_path_and_device() {
        let fault = CgroupFault::of(
            &FaultType::DiskIoThrottle {
                bytes_per_sec: 1024,
                write_only: true,
            },
            || "259:0".to_string(),
        )
        .unwrap();
        let reversal = fault.reversal(vec![Written {
            instance: "default__web-0".into(),
            cgroup: PathBuf::from("/sys/fs/cgroup/web"),
            saved: "/sys/fs/cgroup/web".into(),
        }]);
        assert!(matches!(
            reversal,
            FaultReversal::DiskIo { instances }
                if instances == vec![("/sys/fs/cgroup/web".to_string(), "259:0".to_string())]
        ));
    }

    #[test]
    fn signal_faults_are_not_cgroup_faults() {
        assert_eq!(CgroupFault::of(&FaultType::Pause, String::new), None);
    }
}

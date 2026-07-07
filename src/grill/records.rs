//! On-disk instance records for workload adoption.
//!
//! `exec()` (self-upgrade) and plain crashes both destroy the in-memory
//! bookkeeping — `tokio::process::Child` handles, entry maps — while the
//! workload processes themselves keep running. Each successfully started
//! instance therefore gets a record at `{data_dir}/instances/{id}.json`
//! with everything a fresh bun needs to *adopt* the still-running workload
//! instead of restarting it.
//!
//! A record identifies its process by pid **and** process start time:
//! pids get reused, and adopting an innocent bystander process because it
//! inherited a dead workload's pid would be a spectacular bug.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::app::AppSpec;

use super::oci::OciSpec;

/// Which runtime started the instance (adoption is runtime-specific).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeKind {
    Process,
    Runc,
    Apple,
}

/// Everything needed to adopt one running workload instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRecord {
    /// Record format version. Currently 1.
    pub schema: u32,
    /// Instance id string, e.g. `web-0`.
    pub instance_id: String,
    pub namespace: String,
    pub app_name: String,
    /// Replica index within the app (the `0` of `web-0`).
    pub replica_index: u32,
    /// Job (run-to-completion) rather than long-running app.
    pub is_job: bool,
    /// OCI image reference (informational; empty for process workloads).
    pub image: String,
    pub runtime: RuntimeKind,
    /// ProcessGrill: the workload pid. RunC: the foreground `runc run` pid.
    pub pid: u32,
    /// Process start time (seconds since boot/epoch as reported by the OS)
    /// for pid-reuse detection.
    pub pid_started_at: u64,
    /// RunC container id, if this is a runc instance.
    pub runc_container_id: Option<String>,
    /// Base path for log files (`{stem}.stdout` / `{stem}.stderr`), when
    /// the runtime writes logs to files (required for them to survive exec).
    pub log_stem: Option<PathBuf>,
    /// Host port allocated to this instance, if any (re-reserved on adopt).
    pub host_port: Option<u16>,
    /// The app spec the instance was started from — enough to rebuild
    /// health checks and restart policy after adoption.
    pub app_spec: Option<AppSpec>,
    /// The OCI spec the instance was started with.
    pub oci_spec: OciSpec,
}

/// Record path for an instance id within the records directory.
pub fn record_path(records_dir: &Path, instance_id: &str) -> PathBuf {
    records_dir.join(format!("{instance_id}.json"))
}

/// Persist a record atomically (temp + rename).
pub fn write_record(records_dir: &Path, record: &InstanceRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(records_dir)?;
    let path = record_path(records_dir, &record.instance_id);
    let tmp = records_dir.join(format!(
        ".{}.tmp-{}",
        record.instance_id,
        std::process::id()
    ));
    // A record is plain data; serialisation cannot fail.
    let json = serde_json::to_string_pretty(record).expect("record serialises");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove an instance's record. Missing records are fine (idempotent).
pub fn remove_record(records_dir: &Path, instance_id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(record_path(records_dir, instance_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Load every parseable record in the directory. Unparseable files are
/// skipped with a warning — a corrupt record must not block the others.
pub fn load_records(records_dir: &Path) -> Vec<InstanceRecord> {
    let entries = match std::fs::read_dir(records_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<InstanceRecord>(&s).ok())
        {
            Some(record) => records.push(record),
            None => {
                eprintln!(
                    "bun: warning: skipping unreadable instance record {}",
                    path.display()
                );
            }
        }
    }
    records
}

/// The start time of a live process, or `None` if it doesn't exist.
pub fn process_start_time(pid: u32) -> Option<u64> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|p| p.start_time())
}

/// Is the recorded process still the process we started?
///
/// True only if the pid exists AND its start time matches the record
/// (±2s slack — some platforms round start times differently depending
/// on when they're asked).
pub fn is_live(record: &InstanceRecord) -> bool {
    match process_start_time(record.pid) {
        Some(started_at) => started_at.abs_diff(record.pid_started_at) <= 2,
        None => false,
    }
}

/// Poll a process we have no `Child` handle for (an adoptee).
///
/// After an `exec()` the adopted pids are still our children (exec keeps
/// the process, and children with it), so `waitpid` both detects exit and
/// reaps the zombie. After a full restart (supervisor respawned us) they
/// were reparented elsewhere and `waitpid` gives `ECHILD`; fall back to
/// `kill(pid, 0)` liveness, where the exit code is unknowable.
///
/// Returns `(running, exit_code)`.
pub fn poll_adopted_process(pid: u32) -> (bool, Option<i32>) {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => (true, None),
        Ok(WaitStatus::Exited(_, code)) => (false, Some(code)),
        Ok(_) => (false, None),
        Err(Errno::ECHILD) => match kill(nix_pid, None) {
            Ok(()) => (true, None),
            Err(_) => (false, None),
        },
        Err(_) => (false, None),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciUser};

    fn oci_spec() -> OciSpec {
        OciSpec {
            root: OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: OciProcess {
                args: vec!["sleep".to_string(), "60".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: OciUser { uid: 0, gid: 0 },
            },
            mounts: vec![],
            linux: OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        }
    }

    fn record(pid: u32, started_at: u64) -> InstanceRecord {
        InstanceRecord {
            schema: 1,
            instance_id: "web-0".to_string(),
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            replica_index: 0,
            is_job: false,
            image: String::new(),
            runtime: RuntimeKind::Process,
            pid,
            pid_started_at: started_at,
            runc_container_id: None,
            log_stem: None,
            host_port: Some(30123),
            app_spec: None,
            oci_spec: oci_spec(),
        }
    }

    #[test]
    fn record_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let original = record(4242, 1000);
        write_record(dir.path(), &original).unwrap();

        let loaded = load_records(dir.path());
        assert_eq!(loaded, vec![original]);
    }

    #[test]
    fn remove_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &record(4242, 1000)).unwrap();
        remove_record(dir.path(), "web-0").unwrap();
        remove_record(dir.path(), "web-0").unwrap();
        assert!(load_records(dir.path()).is_empty());
    }

    #[test]
    fn load_records_skips_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &record(4242, 1000)).unwrap();
        std::fs::write(dir.path().join("broken.json"), "not json").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not a record").unwrap();

        assert_eq!(load_records(dir.path()).len(), 1);
    }

    #[test]
    fn load_records_from_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_records(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn liveness_rejects_dead_pid() {
        // Spawn and immediately reap a process so its pid is dead.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!is_live(&record(pid, 1000)));
    }

    #[test]
    fn liveness_rejects_reused_pid() {
        // Our own pid is alive, but the recorded start time is from another
        // era: this must read as "not the process we started".
        let my_pid = std::process::id();
        let real_start = process_start_time(my_pid).unwrap();
        assert!(!is_live(&record(my_pid, real_start + 3600)));
    }

    #[test]
    fn liveness_accepts_matching_pid_and_start_time() {
        let my_pid = std::process::id();
        let real_start = process_start_time(my_pid).unwrap();
        assert!(is_live(&record(my_pid, real_start)));
    }
}

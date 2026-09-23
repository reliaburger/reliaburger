//! Foreground workload owner that outlives Bun and retains kernel child identity.
//!
//! The internal Bun helper runs synchronously before Tokio starts. It is the
//! only reaper of its children. User code waits behind an execution gate until
//! the owner has durably recorded the exact child; no recovered PID is signalled.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

pub(crate) mod exec;

const RECORD_LIMIT: u64 = 1024 * 1024;

/// Durable evidence for one foreground execution generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerPhase {
    /// No gate has been authorised to execute user code.
    Prepared,
    /// Preparation was cancelled while holding the exclusive owner lock.
    Cancelled,
    /// The child identity was persisted before its execution gate opened.
    Running {
        /// Informational PID; only the live owner may use it for signalling.
        pid: u32,
    },
    /// Every supported child is gone; only the control socket needs retirement.
    Retiring {
        /// Actual root exit code retained while metadata cleanup completes.
        exit_code: Option<i32>,
    },
    /// The owner observed root exit and confirmed every supported child absent.
    Retired {
        /// Actual root exit code, or no code when terminated by a signal.
        exit_code: Option<i32>,
    },
}

/// Private launch input and latest durable evidence, owned by one helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerRecord {
    /// Owner record format, independent of agent adoption records.
    pub schema: u32,
    /// Kernel boot that admitted this execution; never inferred from PIDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Unpredictable generation capability used by the control socket.
    pub nonce: String,
    /// Foreground executable followed by its arguments.
    pub command: Vec<String>,
    /// Environment overrides inherited by the foreground executable.
    pub environment: BTreeMap<String, String>,
    /// Last durably confirmed execution phase.
    pub phase: OwnerPhase,
    /// Production runtime intent, persisted before starting any owner helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<ProcessLaunch>,
}

/// Workload identity and complete runtime input for discovery before adoption.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLaunch {
    /// Runtime instance identity whose directory contains this record.
    pub instance_id: super::InstanceId,
    /// Runtime specification retained across Bun death.
    pub spec: super::oci::OciSpec,
}

/// Short private socket location independent of the node's data path length.
pub(crate) fn socket_path(directory: &Path, record: &OwnerRecord) -> PathBuf {
    if record.launch.is_some() {
        PathBuf::from(format!(
            "/tmp/rbp-{}-{}",
            nix::unistd::geteuid(),
            record.nonce
        ))
        .join("control.sock")
    } else {
        directory.join("control.sock")
    }
}

pub(crate) fn load(directory: &Path) -> io::Result<OwnerRecord> {
    let record: OwnerRecord = crate::durable::read_json(
        &directory.join("owner.json"),
        RECORD_LIMIT,
        crate::durable::Access::Regular,
    )?;
    if !matches!(record.schema, 1..=3)
        || record.nonce.is_empty()
        || record.nonce.len() > 128
        || record
            .command
            .first()
            .is_none_or(|command| command.is_empty())
    {
        return Err(io::Error::other("invalid process owner record"));
    }
    if (record.schema >= 2) != record.launch.is_some()
        || (record.schema >= 2
            && (record.nonce.len() != 32
                || !record.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())))
    {
        return Err(io::Error::other("invalid process launch generation"));
    }
    validate_boot(&record)?;
    Ok(record)
}

/// Read the identity of the running kernel boot, in lowercase UUID form.
///
/// Linux publishes a random `boot_id`; macOS publishes `kern.bootsessionuuid`.
/// Both change on every boot, so a record naming another boot cannot have a
/// live owner or child.
pub(crate) fn current_boot_id() -> io::Result<Option<String>> {
    #[cfg(target_os = "linux")]
    let boot = {
        let mut bytes = String::new();
        File::open("/proc/sys/kernel/random/boot_id")?
            .take(64)
            .read_to_string(&mut bytes)?;
        bytes
    };
    #[cfg(target_os = "macos")]
    let boot = {
        let mut bytes = [0u8; 64];
        let mut length = bytes.len();
        // SAFETY: the name is a NUL-terminated literal; the output buffer is
        // writable for `length` bytes and the kernel writes at most that many,
        // updating `length`. No new value is supplied, so nothing is changed.
        let result = unsafe {
            nix::libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = bytes
            .get(..length)
            .ok_or_else(|| io::Error::other("invalid kernel boot identity length"))?;
        String::from_utf8_lossy(value)
            .trim_end_matches('\0')
            .to_owned()
    };
    let boot = boot.trim().to_ascii_lowercase();
    if !valid_boot_id(&boot) {
        return Err(io::Error::other("invalid kernel boot identity"));
    }
    Ok(Some(boot))
}

pub(crate) fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn validate_boot(record: &OwnerRecord) -> io::Result<()> {
    if record
        .boot_id
        .as_deref()
        .is_some_and(|boot| !valid_boot_id(boot))
        || (record.schema == 3 && record.boot_id.is_none())
    {
        return Err(io::Error::other("invalid process owner boot identity"));
    }
    Ok(())
}

pub(crate) fn from_previous_boot(record: &OwnerRecord) -> io::Result<bool> {
    validate_boot(record)?;
    match (&record.boot_id, current_boot_id()?) {
        (Some(original), Some(current)) => Ok(*original != current),
        _ => Ok(false),
    }
}

pub(crate) fn persist(directory: &Path, record: &OwnerRecord) -> io::Result<()> {
    let bytes = serde_json::to_vec(record)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(io::Error::other("process owner record exceeds size limit"));
    }
    crate::sesame::identity::atomic_write_mode(&directory.join("owner.json"), &bytes, Some(0o600))
}

fn owner_executable() -> io::Result<PathBuf> {
    // A Linux owner can outlive atomic replacement or unlinking of Bun's file.
    // Execute its mapped image, not the obsolete pathname returned by readlink.
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}

/// Bootstrap a durable owner, then exit so host init owns its reaping.
///
/// Only the hidden pre-Tokio Bun command calls this. The runtime waits for this
/// bootstrapper to exit before acknowledging start, so a later Bun `exec` cannot
/// discard the only waiter for a long-lived owner child.
pub fn launch_detached_owner(directory: &Path, generation: &str) -> io::Result<()> {
    let _child = Command::new(owner_executable()?)
        .args(["__process-owner", "--directory"])
        .arg(directory)
        .arg("--generation")
        .arg(generation)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;
    std::process::exit(0);
}

/// Run the internal owner on a single thread, before constructing any runtime.
///
/// The directory must already contain its private launch record. A duplicate
/// helper refuses the live lock or a non-prepared generation before launching.
pub fn run_owner(directory: &Path) -> io::Result<()> {
    run_owner_generation(directory, None)
}

/// Run only the generation selected by the launching runtime. Delayed helpers
/// cannot accidentally activate a replacement after cancellation or restart.
pub fn run_owner_generation(directory: &Path, generation: Option<&str>) -> io::Result<()> {
    let lock = lock_owner(directory)?;
    let mut record = load(directory)?;
    if generation.is_some_and(|generation| generation != record.nonce)
        || (record.schema >= 2 && generation.is_none())
    {
        return Err(io::Error::other("process owner generation mismatch"));
    }
    if from_previous_boot(&record)? {
        return Err(io::Error::other(
            "process generation belongs to a previous kernel boot",
        ));
    }
    run_locked_owner(directory, &mut record, lock)
}

pub(crate) fn lock_owner(directory: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(directory.join("owner.lock"))?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "process owner is busy")
        }
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(lock)
}

fn run_locked_owner(directory: &Path, record: &mut OwnerRecord, _lock: File) -> io::Result<()> {
    if !matches!(record.phase, OwnerPhase::Prepared) {
        return Err(io::Error::other(
            "process owner generation has already started",
        ));
    }
    become_subreaper()?;
    exec::install_cancellation_handler()?;
    let socket_path = socket_path(directory, record);
    if record.launch.is_some() {
        use std::os::unix::fs::DirBuilderExt;
        let parent = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("invalid socket path"))?;
        // A previous owner that died before activation may leave this socket.
        // The exclusive lock and Prepared phase fence all delayed launchers.
        match std::fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_socket_directory(parent)?;
                remove_socket(&socket_path)?;
            }
            Err(error) => return Err(error),
        }
    }
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    let log = |suffix: &str| -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join(format!("output.{suffix}")))
    };
    let child = Command::new(owner_executable()?)
        .args(["__process-exec-gate", "--directory"])
        .arg(directory)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(log("stdout")?)
        .stderr(log("stderr")?)
        .spawn()?;
    let mut owned = OwnedChild {
        child,
        reaped: false,
    };
    record.phase = OwnerPhase::Running {
        pid: owned.child.id(),
    };
    persist(directory, record)?;
    // Spawn returns after the gate executable starts, before the user's code.
    // Its private stdin closes without activation if this owner dies here.
    let mut activation = owned
        .child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("execution gate has no activation pipe"))?;
    activation.write_all(b"activate\n")?;
    drop(activation);

    let mut exit_code = None;
    let mut executions = Vec::new();
    loop {
        if exec::cancelled() {
            owned.signal(Signal::SIGKILL)?;
        }
        match listener.accept() {
            Ok((connection, _)) => {
                // An abandoned or malformed client must not end the owner.
                let _ = respond(connection, directory, record, &owned, &mut executions);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        reap_orphans(owned.child.id(), &executions)?;
        if exit_code.is_none() {
            exit_code = observe_exit(owned.child.id())?;
        }
        let executions_retired = exec::poll(&mut executions, exit_code.is_some())?;
        if let Some(code) = exit_code
            && executions_retired
            && retire_children(&mut owned)?
        {
            record.phase = OwnerPhase::Retiring { exit_code: code };
            persist(directory, record)?;
            drop(listener);
            complete_retirement(directory, record)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether no process remains in the execution gate's process group.
///
/// The gate leads its own group, so its PID is also the group ID. This sends
/// the null signal, which only asks the kernel whether the group exists, so it
/// can never disturb a process that later reuses the ID. A reused group, or a
/// zombie awaiting its reaper, reads as present; the caller just retries.
pub(crate) fn process_group_absent(leader: u32) -> io::Result<bool> {
    let group =
        i32::try_from(leader).map_err(|_| io::Error::other("invalid recorded process group"))?;
    match nix::sys::signal::killpg(Pid::from_raw(group), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(false),
        Err(nix::errno::Errno::ESRCH) => Ok(true),
        Err(error) => Err(io::Error::from(error)),
    }
}

pub(crate) fn complete_retirement(directory: &Path, record: &mut OwnerRecord) -> io::Result<()> {
    let OwnerPhase::Retiring { exit_code } = record.phase else {
        return Err(io::Error::other("process retirement has no absence proof"));
    };
    let socket = socket_path(directory, record);
    if record.launch.is_some() {
        let parent = socket
            .parent()
            .ok_or_else(|| io::Error::other("invalid socket path"))?;
        match validate_socket_directory(parent) {
            Ok(()) => {
                remove_socket(&socket)?;
                std::fs::remove_dir(parent)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    } else {
        remove_socket(&socket)?;
    }
    record.phase = OwnerPhase::Retired { exit_code };
    persist(directory, record)
}

pub(crate) fn validate_socket_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(io::Error::other("invalid private process socket directory"));
    }
    Ok(())
}

pub(crate) fn remove_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(io::Error::other("unexpected file at process owner socket")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Run the child's internal gate and replace it with the foreground command.
///
/// EOF, malformed activation or a mismatched durable child identity refuses
/// before user code. The gate PID remains the workload PID across `exec`.
pub fn run_execution_gate(directory: &Path) -> io::Result<()> {
    let mut activation = [0u8; 9];
    io::stdin()
        .read_exact(&mut activation)
        .map_err(|error| io::Error::other(format!("execution activation failed: {error}")))?;
    if &activation != b"activate\n" {
        return Err(io::Error::other("invalid execution activation"));
    }
    let record = load(directory)?;
    if !matches!(record.phase, OwnerPhase::Running { pid } if pid == std::process::id()) {
        return Err(io::Error::other(
            "execution activation has no matching durable owner",
        ));
    }
    let error = Command::new(&record.command[0])
        .args(&record.command[1..])
        .envs(&record.environment)
        .stdin(Stdio::null())
        .exec();
    Err(error)
}

struct OwnedChild {
    child: Child,
    reaped: bool,
}

impl OwnedChild {
    fn signal(&self, signal: Signal) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        // No other thread reaps this child, so its group identifier cannot
        // be recycled between observing exit and sending the signal.
        match kill(Pid::from_raw(-(self.child.id() as i32)), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            // macOS can refuse signals to a zombie-only group. The retained
            // root proves its exit, but complete retirement still checks every
            // group member before the owner publishes absence.
            #[cfg(target_os = "macos")]
            Err(nix::errno::Errno::EPERM) if observe_exit(self.child.id())?.is_some() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal(Signal::SIGKILL);
            // An error never publishes Retired; durable uncertainty remains.
        }
    }
}

/// None means no exit yet; Some(None) means an observed signal termination.
fn observe_exit(pid: u32) -> io::Result<Option<Option<i32>>> {
    let mut info = std::mem::MaybeUninit::<nix::libc::siginfo_t>::zeroed();
    // SAFETY: the caller exclusively owns this unreaped child. The POD output
    // has its exact C layout and valid zeroed storage; WNOHANG bounds the call.
    let result = unsafe {
        nix::libc::waitid(
            nix::libc::P_PID,
            pid as nix::libc::id_t,
            info.as_mut_ptr(),
            nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded with a zero-initialised siginfo_t. Reading the
    // child-event fields is valid; zero si_pid represents no available event.
    let info = unsafe { info.assume_init() };
    // SAFETY: successful waitid initialised the child-event discriminator.
    let observed = unsafe { info.si_pid() };
    if observed == 0 {
        return Ok(None);
    }
    if observed != pid as i32 {
        return Err(io::Error::other("unexpected child exit identity"));
    }
    // SAFETY: si_pid identified this child exit event, whose status is valid.
    let code = (info.si_code == nix::libc::CLD_EXITED).then(|| unsafe { info.si_status() });
    Ok(Some(code))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    nonce: String,
    action: Action,
    #[serde(default)]
    command: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Status,
    Exec,
    Terminate,
    Kill,
}

fn respond(
    mut socket: UnixStream,
    directory: &Path,
    record: &OwnerRecord,
    child: &OwnedChild,
    executions: &mut Vec<exec::Execution>,
) -> io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    socket.set_write_timeout(Some(Duration::from_millis(100)))?;
    let mut bytes = Vec::new();
    BufReader::new(socket.try_clone()?)
        .take(exec::REQUEST_LIMIT as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    let response = if bytes.len() > exec::REQUEST_LIMIT || bytes.last() != Some(&b'\n') {
        serde_json::json!({"error": "invalid owner request size"})
    } else {
        match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) if request.nonce != record.nonce => {
                serde_json::json!({"error": "owner generation mismatch"})
            }
            Ok(request) => match request.action {
                Action::Status => serde_json::json!({"phase": record.phase}),
                Action::Exec => {
                    if observe_exit(child.child.id())?.is_some() || exec::cancelled() {
                        serde_json::json!({"error": "workload is stopping"})
                    } else {
                        match exec::Execution::launch(
                            directory,
                            record,
                            request.command,
                            socket.try_clone()?,
                            executions,
                        ) {
                            Ok(()) => return Ok(()),
                            Err(error) => serde_json::json!({"error": error.to_string()}),
                        }
                    }
                }
                action => match child.signal(match action {
                    Action::Terminate => Signal::SIGTERM,
                    _ => Signal::SIGKILL,
                }) {
                    Ok(()) => serde_json::json!({"accepted": true}),
                    Err(error) => serde_json::json!({"error": error.to_string()}),
                },
            },
            Err(_) => serde_json::json!({"error": "invalid owner request"}),
        }
    };
    serde_json::to_writer(&mut socket, &response)?;
    socket.write_all(b"\n")
}

#[cfg(target_os = "linux")]
fn become_subreaper() -> io::Result<()> {
    // SAFETY: this helper is single-threaded and owns no unrelated children.
    // The scalar prctl operation makes orphaned descendants its own children.
    let result = unsafe { nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn become_subreaper() -> io::Result<()> {
    Ok(())
}

/// Reap exited orphans that the subreaper adopted while the workload runs.
///
/// A double-forking workload hands its grandchildren to this owner. Without
/// reaping, each one stays a zombie until the whole generation retires. The
/// root child and exec helpers are left alone: `observe_exit` and each
/// execution still need their exit statuses, and reaping would discard them.
#[cfg(target_os = "linux")]
fn reap_orphans(root: u32, executions: &[exec::Execution]) -> io::Result<()> {
    use nix::sys::wait::{Id, WaitPidFlag, waitid, waitpid};
    loop {
        // WNOWAIT only peeks, so a tracked child's status stays in place.
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        let pid = match waitid(Id::All, flags) {
            Ok(status) => status.pid(),
            Err(nix::errno::Errno::ECHILD) => None,
            Err(error) => return Err(error.into()),
        };
        let Some(pid) = pid else {
            return Ok(());
        };
        let raw = pid.as_raw() as u32;
        if raw == root
            || executions
                .iter()
                .any(|execution| execution.child_id() == Some(raw))
        {
            // Its own waiter collects it this tick; orphans wait for the next.
            return Ok(());
        }
        // Only this single-threaded owner reaps, so the peeked PID is still
        // that exited orphan.
        waitpid(pid, Some(WaitPidFlag::WNOHANG))?;
    }
}

/// macOS has no subreaper, so launchd adopts and reaps orphaned descendants.
#[cfg(target_os = "macos")]
fn reap_orphans(_root: u32, _executions: &[exec::Execution]) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn retire_children(owner: &mut OwnedChild) -> io::Result<bool> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    if !owner.reaped {
        owner.signal(Signal::SIGKILL)?;
    }
    // The subreaper can acquire grandchildren after their parents die. The
    // list locates children to signal; only waitpid(ECHILD) proves completion.
    let children =
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
    for value in children.split_whitespace() {
        let pid: u32 = value.parse().map_err(io::Error::other)?;
        match observe_exit(pid) {
            Ok(None) => {
                // It remains our unreaped child, even if it exits now.
                match kill(Pid::from_raw(pid as i32), Signal::SIGKILL) {
                    Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some(_)) => {}
            Err(error) if error.raw_os_error() == Some(nix::libc::ECHILD) => {}
            Err(error) => return Err(error),
        }
    }
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _)) => {
                if pid.as_raw() == owner.child.id() as i32 {
                    owner.reaped = true;
                }
            }
            Err(nix::errno::Errno::ECHILD) => return Ok(true),
            Ok(_) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(target_os = "macos")]
fn retire_children(owner: &mut OwnedChild) -> io::Result<bool> {
    // XNU snapshots matching live and zombie process IDs under proc_list_lock.
    // A short buffer cannot establish absence: grow and retry instead.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 64];
    loop {
        let capacity = std::mem::size_of_val(pids.as_slice());
        // SAFETY: the vector provides aligned writable storage of capacity
        // bytes. The retained child pins the queried process-group identity.
        let bytes = unsafe {
            nix::libc::proc_listpids(
                PROC_PGRP_ONLY,
                owner.child.id(),
                pids.as_mut_ptr().cast(),
                capacity as i32,
            )
        };
        if bytes <= 0 {
            return Err(io::Error::other("cannot inspect owned process group"));
        }
        let bytes = bytes as usize;
        if bytes > capacity || !bytes.is_multiple_of(std::mem::size_of::<i32>()) {
            return Err(io::Error::other("invalid process group snapshot size"));
        }
        if bytes == capacity {
            if pids.len() >= 1_048_576 {
                return Err(io::Error::other("process group snapshot exceeds limit"));
            }
            pids.resize(pids.len() * 2, 0);
            continue;
        }
        pids.truncate(bytes / std::mem::size_of::<i32>());
        if !pids.contains(&(owner.child.id() as i32)) {
            return Err(io::Error::other(
                "owned child missing from process group snapshot",
            ));
        }
        if pids
            .iter()
            .any(|pid| *pid != 0 && *pid != owner.child.id() as i32)
        {
            // Zombie-only groups can refuse signals on macOS. Their members
            // must still disappear from the snapshot before retirement.
            match owner.signal(Signal::SIGKILL) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(nix::libc::EPERM) => {}
                Err(error) => return Err(error),
            }
            return Ok(false);
        }
        owner.child.wait()?;
        owner.reaped = true;
        return Ok(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(boot_id: Option<String>) -> OwnerRecord {
        OwnerRecord {
            schema: 1,
            boot_id,
            nonce: "test-generation".into(),
            command: vec!["true".into()],
            environment: BTreeMap::new(),
            phase: OwnerPhase::Prepared,
            launch: None,
        }
    }

    #[test]
    fn host_has_a_stable_valid_boot_identity() {
        let first = current_boot_id().unwrap().expect("host exposes no boot id");
        assert!(valid_boot_id(&first), "{first}");
        assert_eq!(first, first.to_ascii_lowercase());
        assert_eq!(current_boot_id().unwrap(), Some(first));
    }

    #[test]
    fn record_from_another_boot_is_from_a_previous_boot() {
        let current = current_boot_id().unwrap();
        assert!(!from_previous_boot(&record(current)).unwrap());
        let other = "00000000-0000-4000-8000-000000000000".to_owned();
        assert!(from_previous_boot(&record(Some(other))).unwrap());
    }
}

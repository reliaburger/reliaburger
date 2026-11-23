//! Auxiliary commands remain children of the workload's durable supervisor.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Maximum encoded owner request, including its newline.
pub(crate) const REQUEST_LIMIT: usize = 64 * 1024;
/// Bound for JSON-escaped output and response framing.
pub(crate) const RESPONSE_LIMIT: u64 = 6 * 1024 * 1024 + 1024;
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const EXEC_LIMIT: usize = 16;
/// How long the owner lets an exec run before it retires the command itself.
///
/// It's a backstop: the agent's deadline (at most
/// `bun::agent::MAX_EXEC_TIMEOUT`) drops the socket first, which cancels the
/// command. Above the agent's cap and the owner-socket wait, so neither
/// hears a closed socket where it should have heard its own named timeout
/// (#2128).
pub(crate) const OWNER_EXEC_DEADLINE: Duration = Duration::from_secs(320);
static CANCELLED: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel(_: nix::libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

/// Let a retained parent request group retirement without killing its owner.
pub(super) fn install_cancellation_handler() -> io::Result<()> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(cancel),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: this pre-Tokio helper is single-threaded. The handler only stores
    // a lock-free atomic flag; it does not allocate, lock or access child handles.
    unsafe { sigaction(Signal::SIGTERM, &action) }.map_err(io::Error::from)?;
    Ok(())
}

/// Whether this owner has received a cancellation signal.
pub(super) fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

/// Retained helper identity, caller connection and cleanup obligation.
pub(super) struct Execution {
    directory: PathBuf,
    child: Option<Child>,
    socket: Option<UnixStream>,
    deadline: Instant,
    reply: Option<(Vec<u8>, usize)>,
    retired: bool,
}

impl Execution {
    /// Persist intent and retain a child owner before accepting execution.
    pub(super) fn launch(
        parent: &Path,
        parent_record: &OwnerRecord,
        command: Vec<String>,
        socket: UnixStream,
        pending: &mut Vec<Self>,
    ) -> io::Result<()> {
        use ring::rand::{SecureRandom, SystemRandom};
        use std::os::unix::fs::PermissionsExt;
        if pending.len() >= EXEC_LIMIT {
            return Err(io::Error::other("too many concurrent exec commands"));
        }
        if command.first().is_none_or(|value| value.is_empty()) {
            return Err(io::Error::other("no exec command specified"));
        }
        socket.set_nonblocking(true)?;
        let temporary = tempfile::Builder::new()
            .prefix("exec-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let mut nonce = [0u8; 16];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| io::Error::other("cannot generate exec capability"))?;
        let mut launch = parent_record.launch.clone();
        launch.spec.process.args = command.clone();
        launch.spec.process.env.clear();
        launch.spec.port_mapping = None;
        let record = OwnerRecord {
            schema: 3,
            boot_id: super::current_boot_id()?
                .ok_or_else(|| io::Error::other("kernel boot identity unavailable"))?,
            nonce: hex::encode(nonce),
            command,
            // An exec runs inside its workload, as `docker exec` does, so it
            // gets the workload's complete recorded environment: Bun's
            // allowlisted variables plus the workload's own. The gate clears
            // everything else.
            environment: parent_record.environment.clone(),
            phase: OwnerPhase::Prepared,
            launch,
            gate: None,
        };
        persist(temporary.path(), &record)?;
        File::open(parent)?.sync_all()?;
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(temporary.path().join("owner.log"))?;
        let child = Command::new(owner_executable()?)
            .args(["__process-owner", "--directory"])
            .arg(temporary.path())
            .arg("--generation")
            .arg(&record.nonce)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        // No fallible operation separates spawn from retaining the only waiter.
        pending.push(Self {
            directory: temporary.keep(),
            child: Some(child),
            socket: Some(socket),
            deadline: Instant::now() + OWNER_EXEC_DEADLINE,
            reply: None,
            retired: false,
        });
        Ok(())
    }

    /// The helper's PID while it remains this owner's unreaped child.
    #[cfg(target_os = "linux")]
    pub(super) fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn cancel(&self) -> io::Result<()> {
        if let Some(child) = &self.child {
            // This unreaped child is our helper, not a PID recovered from disk.
            // SIGTERM asks it to retire its own group before committing absence.
            match kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn read_output(&self) -> io::Result<String> {
        let mut output = String::new();
        let mut total = 0;
        for suffix in ["stdout", "stderr"] {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(self.directory.join(format!("output.{suffix}")))?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("invalid exec output file"));
            }
            let mut bytes = Vec::new();
            file.take(OUTPUT_LIMIT + 1 - total)
                .read_to_end(&mut bytes)?;
            total += bytes.len() as u64;
            if total > OUTPUT_LIMIT {
                return Err(io::Error::other("exec output exceeds 1 MiB limit"));
            }
            if !output.is_empty() && !output.ends_with('\n') && !bytes.is_empty() {
                output.push('\n');
            }
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
        Ok(output)
    }

    fn queue_reply(&mut self, value: serde_json::Value) -> io::Result<()> {
        if self.reply.is_none() && self.socket.is_some() {
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            self.reply = Some((bytes, 0));
        }
        Ok(())
    }

    fn finish_child(&mut self) -> io::Result<()> {
        if self.retired {
            return Ok(());
        }
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                return Ok(());
            }
            self.child = None;
        }
        let _lock = lock_owner(&self.directory)?;
        let mut record = load(&self.directory)?;
        match record.phase {
            OwnerPhase::Prepared => {
                record.phase = OwnerPhase::Cancelled;
                persist(&self.directory, &record)?;
                // Prepared proves that no user command received activation.
                let path = socket_path(&record);
                if let Some(parent) = path.parent()
                    && parent.exists()
                {
                    validate_socket_directory(parent)?;
                    remove_socket(&path)?;
                    std::fs::remove_dir(parent)?;
                }
            }
            OwnerPhase::Retiring { .. } => complete_retirement(&self.directory, &mut record)?,
            OwnerPhase::Retired { .. } | OwnerPhase::Cancelled => {}
            OwnerPhase::Running { .. } => {
                return Err(io::Error::other(
                    "exec owner disappeared without retirement proof",
                ));
            }
        }
        self.retired = true;
        let response = match match record.phase {
            OwnerPhase::Retired {
                exit_code: Some(exit_code),
            } => self.read_output().map(|output| (output, exit_code)),
            _ => Err(io::Error::other("exec command was interrupted")),
        } {
            Ok((output, exit_code)) => {
                serde_json::json!({"output": output, "exit_code": exit_code})
            }
            Err(error) => serde_json::json!({"error": error.to_string()}),
        };
        self.queue_reply(response)
    }

    fn poll(&mut self, parent_stopped: bool) -> io::Result<bool> {
        if let Some(socket) = self.socket.as_mut() {
            let mut byte = [0u8; 1];
            match socket.read(&mut byte) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                // EOF, unexpected request bytes and failures all cancel execution.
                _ => self.socket = None,
            }
        }
        if let Err(error) = self.finish_child() {
            // Retain the failed generation. Reporting an error to its caller
            // cannot discharge the parent's cleanup obligation.
            self.queue_reply(serde_json::json!({"error": error.to_string()}))?;
        }
        if parent_stopped || self.socket.is_none() || Instant::now() >= self.deadline {
            self.cancel()?;
        }
        if let (Some(socket), Some((bytes, offset))) = (&mut self.socket, &mut self.reply) {
            match socket.write(&bytes[*offset..]) {
                Ok(0) => self.socket = None,
                Ok(size) => {
                    *offset += size;
                    if *offset == bytes.len() {
                        self.socket = None;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => self.socket = None,
            }
        }
        if Instant::now() >= self.deadline {
            self.socket = None;
        }
        if self.retired && (parent_stopped || self.socket.is_none()) {
            std::fs::remove_dir_all(&self.directory)?;
            return Ok(true);
        }
        Ok(false)
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

/// Progress every command without letting one stalled caller block the owner.
pub(super) fn poll(pending: &mut Vec<Execution>, parent_stopped: bool) -> io::Result<bool> {
    let mut index = 0;
    while index < pending.len() {
        match pending[index].poll(parent_stopped) {
            Ok(true) => {
                pending.swap_remove(index);
            }
            Ok(false) => {
                index += 1;
            }
            Err(error) => {
                // A failed signal or metadata removal retains the child and
                // retries; it must not kill the application's healthy owner.
                pending[index].queue_reply(serde_json::json!({"error": error.to_string()}))?;
                index += 1;
            }
        }
    }
    Ok(pending.is_empty())
}

/// Most processes one helper's report describes, so a runaway tree can't
/// flood the log line it goes into.
#[cfg(target_os = "linux")]
const REPORT_PROCESS_LIMIT: usize = 16;

/// What every exec helper under `parent`, a workload owner's directory, is
/// doing: its phase, its command and, while it runs, the processes under its
/// gate. Each process is `pid N (name) S wchan`, its scheduler state and the
/// kernel function it sleeps in where the system says. Bun logs this when an
/// exec goes unanswered, so a wedged one says where it's stuck (#2128).
///
/// Best-effort and read-only: a helper it can't read is reported as such,
/// and a process that has gone is just missing.
pub(crate) fn describe_helpers(parent: &Path) -> String {
    let mut helpers: Vec<(String, PathBuf)> = match std::fs::read_dir(parent) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let is_directory = entry.file_type().ok()?.is_dir();
                (is_directory && name.starts_with("exec-")).then(|| (name, entry.path()))
            })
            .collect(),
        Err(error) => return format!("exec helpers unreadable ({error})"),
    };
    if helpers.is_empty() {
        return "no exec helper".to_string();
    }
    helpers.sort();
    helpers
        .iter()
        .map(|(name, directory)| match load(directory) {
            Ok(record) => format!("{name}: {}", describe_helper(&record)),
            Err(error) => format!("{name}: unreadable ({error})"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One helper's phase and command, and for a running one its processes.
fn describe_helper(record: &OwnerRecord) -> String {
    let command = format!("`{}`", record.command.join(" "));
    match &record.phase {
        OwnerPhase::Prepared => format!("prepared {command}, its owner hasn't started it"),
        OwnerPhase::Cancelled => format!("cancelled {command}"),
        OwnerPhase::Running { pid } => {
            let gate = match &record.gate {
                None => "no gate report yet",
                Some(GateReport::Closed) => "gate handed over",
                Some(GateReport::Unanswered) => "gate never answered",
                Some(GateReport::Failed { .. }) => "gate failed",
            };
            format!("running {command} ({gate}): {}", describe_processes(*pid))
        }
        OwnerPhase::Retiring { exit_code } => {
            format!("retiring {command} after exit {exit_code:?}")
        }
        OwnerPhase::Retired { exit_code } => format!("retired {command} with exit {exit_code:?}"),
    }
}

/// `root` and everything under it, breadth first, up to
/// [`REPORT_PROCESS_LIMIT`] processes.
#[cfg(target_os = "linux")]
fn describe_processes(root: u32) -> String {
    let mut described = Vec::new();
    let mut queue = std::collections::VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        if described.len() == REPORT_PROCESS_LIMIT {
            described.push("…".to_string());
            break;
        }
        match describe_process(pid) {
            Some(process) => described.push(process),
            None if pid == root => described.push(format!("pid {pid} gone")),
            None => continue,
        }
        queue.extend(child_processes(pid));
    }
    described.join(", ")
}

/// Without procfs there's no portable way to read another process's state,
/// so the report names the gate and stops there.
#[cfg(not(target_os = "linux"))]
fn describe_processes(root: u32) -> String {
    format!("pid {root}")
}

/// `pid N (name) S wchan` from procfs, or `None` once the process has gone.
#[cfg(target_os = "linux")]
fn describe_process(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name sits in parentheses and may hold spaces or parentheses
    // itself, so it ends at the last `)`; the state is the field after it.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?;
    let state = stat.get(close + 1..)?.split_whitespace().next()?;
    let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan")).unwrap_or_default();
    let wchan = match wchan.trim() {
        "" | "0" => String::new(),
        function => format!(" {function}"),
    };
    Some(format!("pid {pid} ({name}) {state}{wchan}"))
}

/// The children of every thread of `pid`: a Go program like runc starts
/// processes from whichever thread it's on.
#[cfg(target_os = "linux")]
fn child_processes(pid: u32) -> Vec<u32> {
    let Ok(threads) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut children: Vec<u32> = threads
        .filter_map(Result::ok)
        .filter_map(|thread| std::fs::read_to_string(thread.path().join("children")).ok())
        .flat_map(|listed| {
            listed
                .split_whitespace()
                .filter_map(|value| value.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect();
    children.sort_unstable();
    children.dedup();
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper directory under `parent` holding `record`, as `launch`
    /// leaves one.
    fn helper(parent: &Path, name: &str, record: &OwnerRecord) {
        use std::os::unix::fs::DirBuilderExt;
        let directory = parent.join(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        persist(&directory, record).unwrap();
    }

    fn helper_record(command: &[&str], phase: OwnerPhase) -> OwnerRecord {
        let mut record = super::super::tests::record(current_boot_id().unwrap().unwrap());
        record.command = command.iter().map(|value| value.to_string()).collect();
        record.phase = phase;
        record
    }

    #[test]
    fn a_report_names_each_helper_its_phase_command_and_process() {
        let parent = tempfile::tempdir().unwrap();
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        helper(
            parent.path(),
            "exec-running",
            &helper_record(&["sleep", "30"], OwnerPhase::Running { pid: sleeper.id() }),
        );
        helper(
            parent.path(),
            "exec-prepared",
            &helper_record(&["true"], OwnerPhase::Prepared),
        );
        // The owner's own files sit beside its helpers and aren't helpers.
        std::fs::write(parent.path().join("owner.json"), "{}").unwrap();

        let report = describe_helpers(parent.path());
        let _ = sleeper.kill();
        let _ = sleeper.wait();

        assert!(
            report.contains("exec-running: running `sleep 30`"),
            "{report}"
        );
        assert!(
            report.contains(&format!("pid {}", sleeper.id())),
            "{report}"
        );
        // The scheduler state and the kernel function it waits in are what
        // tell a sleeping command from one stuck on the disk.
        #[cfg(target_os = "linux")]
        assert!(report.contains("(sleep) S"), "{report}");
        assert!(
            report.contains("exec-prepared: prepared `true`"),
            "{report}"
        );
        assert!(!report.contains("owner.json"), "{report}");
    }

    #[test]
    fn a_report_without_helpers_says_so() {
        let parent = tempfile::tempdir().unwrap();
        assert_eq!(describe_helpers(parent.path()), "no exec helper");
    }

    #[test]
    fn an_unreadable_helper_is_reported_not_skipped() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::create_dir(parent.path().join("exec-broken")).unwrap();
        let report = describe_helpers(parent.path());
        assert!(report.starts_with("exec-broken: unreadable ("), "{report}");
    }

    /// `runc exec` runs its command as a grandchild, so the report follows
    /// the whole tree under the gate, not just the gate.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_report_follows_the_helpers_process_tree() {
        let parent = tempfile::tempdir().unwrap();
        let mut shell = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        helper(
            parent.path(),
            "exec-tree",
            &helper_record(&["sh"], OwnerPhase::Running { pid: shell.id() }),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let report = loop {
            let report = describe_helpers(parent.path());
            if report.contains("(sleep)") || Instant::now() >= deadline {
                break report;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = nix::sys::signal::killpg(Pid::from_raw(shell.id() as i32), Signal::SIGKILL);
        let _ = shell.kill();
        let _ = shell.wait();
        assert!(report.contains("(sh) S"), "{report}");
        assert!(report.contains("(sleep) S"), "{report}");
    }
}

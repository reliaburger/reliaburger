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
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
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
        let mut launch = parent_record
            .launch
            .clone()
            .ok_or_else(|| io::Error::other("exec requires production launch intent"))?;
        launch.spec.process.args = command.clone();
        launch.spec.process.env.clear();
        launch.spec.port_mapping = None;
        let record = OwnerRecord {
            schema: 2,
            nonce: hex::encode(nonce),
            command,
            // Process exec has always inherited Bun's host environment.
            environment: BTreeMap::new(),
            phase: OwnerPhase::Prepared,
            launch: Some(launch),
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
            deadline: Instant::now() + EXEC_TIMEOUT,
            reply: None,
            retired: false,
        });
        Ok(())
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
                let path = socket_path(&self.directory, &record);
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
        let response = match if matches!(record.phase, OwnerPhase::Retired { exit_code: Some(_) }) {
            self.read_output()
        } else {
            Err(io::Error::other("exec command was interrupted"))
        } {
            Ok(output) => serde_json::json!({"output": output}),
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

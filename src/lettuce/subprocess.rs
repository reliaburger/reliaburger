//! Bounded subprocesses for Lettuce's `git` calls (B19).
//!
//! `Command::output()` waits for as long as the child likes. A stalled
//! remote, a credential helper waiting for a password nobody will type, or
//! a hung signature verifier held the only GitOps sync loop forever, and
//! neither the poll timer nor shutdown could get it back. Timing out the
//! `spawn_blocking` task that called `output()` wouldn't have helped
//! either: dropping a future stops *waiting* for the work, not the work
//! itself, so the child would keep running.
//!
//! [`run_bounded`] gives every child an owned, bounded lifetime instead:
//!
//! - it runs in a new session, so it has no controlling terminal to prompt
//!   on, and everything it spawns shares one process group;
//! - git's own prompts are switched off through the environment;
//! - a deadline, a cancellation token and an output cap each stop it;
//! - whenever it stops, for any reason, the whole process group is killed
//!   and the child reaped, then the group is killed again until it is
//!   empty, so no descendant outlives the call, not even one that was being
//!   forked while the first kill went out.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::types::LettuceError;

/// How long one `git` invocation may run by default.
///
/// Generous enough for a first clone of a large repository over a slow
/// link; a remote that stalls for longer than this is broken.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The most stdout or stderr one invocation may produce.
///
/// GitOps reads TOML files and short command output; anything bigger is a
/// runaway process, not a config.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// How long to wait for the output readers once the process group is dead.
///
/// Only a descendant that escaped the group (by starting its own session)
/// can still hold a pipe open by then. We don't wait for it.
const READER_GRACE: Duration = Duration::from_secs(1);

/// How long to keep killing the process group after the child is reaped.
const GROUP_SWEEP_LIMIT: Duration = Duration::from_secs(1);

/// The limits one subprocess runs under.
#[derive(Debug, Clone)]
pub struct CommandBudget {
    /// Kill the child if it runs longer than this.
    pub timeout: Duration,
    /// Kill the child as soon as this is cancelled (node shutdown).
    pub cancel: CancellationToken,
}

impl Default for CommandBudget {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }
}

/// Why a bounded child was stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    TimedOut,
    Cancelled,
    OutputTooLarge,
}

/// Run `command` to completion within `budget`, returning its output.
///
/// `what` names the operation in error messages (`"git fetch"`). The
/// child's exit status is returned, not judged: callers decide what a
/// non-zero exit means. An error means the child never ran, or was stopped
/// by the deadline, cancellation or the output cap.
pub fn run_bounded(
    mut command: Command,
    budget: &CommandBudget,
    what: &str,
) -> Result<Output, LettuceError> {
    configure(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| LettuceError::GitFailed(format!("failed to run {what}: {e}")))?;

    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = spawn_reader(child.stdout.take(), Arc::clone(&overflow));
    let stderr = spawn_reader(child.stderr.take(), Arc::clone(&overflow));

    let stopped = wait_within(&child, budget, &overflow);

    // Kill the group whether the child finished or not: a finished git can
    // still leave a helper or daemon behind, and that helper may hold our
    // pipes open. The child is not reaped yet, so its pid (the group id)
    // can't have been reused. Killing an already-exited child is a no-op,
    // so a clean exit keeps its real status.
    kill_group(&child);
    let status = child
        .wait()
        .map_err(|e| LettuceError::GitFailed(format!("failed to reap {what}: {e}")))?;
    // One kill can miss a process that a member was forking at that moment
    // (and the leader may still have been forking until it was reaped), so
    // keep killing until the group is empty.
    sweep_group(&child);

    // A pipe still open after the group is dead belongs to a descendant
    // that escaped it; its output is incomplete, so it counts as a failure.
    let stdout = collect(stdout);
    let stderr = collect(stderr);
    let stopped = match stopped {
        // The child may exit before its reader notices the overflow.
        Ok(()) if overflow.load(Ordering::SeqCst) => Err(Stop::OutputTooLarge),
        other => other,
    };

    match stopped {
        Ok(()) => {}
        Err(Stop::TimedOut) => {
            return Err(LettuceError::GitFailed(format!(
                "{what} timed out after {}s and was killed",
                budget.timeout.as_secs_f64()
            )));
        }
        Err(Stop::Cancelled) => {
            return Err(LettuceError::GitFailed(format!(
                "{what} was cancelled by shutdown"
            )));
        }
        Err(Stop::OutputTooLarge) => {
            return Err(LettuceError::GitFailed(format!(
                "{what} produced more than {MAX_OUTPUT_BYTES} bytes of output and was killed"
            )));
        }
    }

    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(LettuceError::GitFailed(format!(
            "{what} left a process holding its output open"
        )));
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Make `command` non-interactive and give it its own session.
fn configure(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // git asks on the terminal for missing credentials unless told not
        // to; Git Credential Manager has its own switch.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // OpenSSH would otherwise try an askpass program for a passphrase
        // or host-key question.
        .env("SSH_ASKPASS_REQUIRE", "never");
    // SAFETY: `setsid` is async-signal-safe, which is all `pre_exec`
    // requires of code that runs between fork and exec. It makes the child
    // a session and process-group leader with no controlling terminal, so
    // ssh or gpg can't open /dev/tty to prompt, and `kill_group` reaches
    // every descendant that didn't deliberately leave.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
}

/// Read a pipe to the end on its own thread, flagging `overflow` once it
/// passes [`MAX_OUTPUT_BYTES`]. The result arrives on the returned channel.
fn spawn_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    overflow: Arc<AtomicBool>,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = pipe {
            // Read one byte past the cap so "exactly at the cap" and "over
            // it" are distinguishable. Stopping here drops the pipe, and a
            // child still writing gets EPIPE.
            let limit = MAX_OUTPUT_BYTES as u64 + 1;
            let _ = pipe.take(limit).read_to_end(&mut buffer);
            if buffer.len() > MAX_OUTPUT_BYTES {
                overflow.store(true, Ordering::SeqCst);
            }
        }
        let _ = tx.send(buffer);
    });
    rx
}

/// A reader's output, or `None` if it is still blocked after the grace
/// period (see [`READER_GRACE`]). A blocked reader thread is abandoned; it
/// ends when the escaped process closes the pipe.
fn collect(reader: mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
    reader.recv_timeout(READER_GRACE).ok()
}

/// Poll until the child exits, or until the deadline, cancellation or the
/// output cap says to stop it. Never reaps the child.
fn wait_within(child: &Child, budget: &CommandBudget, overflow: &AtomicBool) -> Result<(), Stop> {
    let deadline = Instant::now() + budget.timeout;
    let mut pause = Duration::from_millis(1);
    loop {
        // A signal can interrupt the check; that says nothing about the
        // child, so look again. Any other error means the pid is no longer
        // our child, which only happens once it has been reaped.
        let exited = match has_exited(child) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.unwrap_or(true),
        };
        if exited {
            return Ok(());
        }
        if overflow.load(Ordering::SeqCst) {
            return Err(Stop::OutputTooLarge);
        }
        if budget.cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Stop::TimedOut);
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

/// Whether `child` has exited, without reaping it.
///
/// `Child::try_wait` would reap it, which frees its pid for reuse while we
/// still need it as the process-group id to kill. `waitid` with `WNOWAIT`
/// only peeks.
fn has_exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: `siginfo_t` is a plain C struct for which all-zero bytes are
    // a valid value; `waitid` only writes into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `siginfo_t` for the whole call,
    // and the flags ask only for a non-blocking, non-reaping status check.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // With WNOHANG, `si_pid` stays zero when the child hasn't changed state.
    // SAFETY: `waitid` succeeded, so `info` holds a valid `siginfo_t`.
    Ok(unsafe { info.si_pid() } != 0)
}

/// SIGKILL the child's whole process group.
fn kill_group(child: &Child) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    // ESRCH (the group is already empty) is the common, harmless case.
    let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
}

/// SIGKILL the reaped child's process group until it has no members left,
/// for at most [`GROUP_SWEEP_LIMIT`].
///
/// The leader is reaped, but its pid stays reserved as the group id for as
/// long as any member is left (POSIX never reuses a pid that still names a
/// process group), so this reaches only our descendants. The first ESRCH
/// ends the sweep, and we never signal that id again. A killed member
/// counts until its new parent reaps it, which is usually at once; the
/// limit only stops a slow reaper from holding the call.
fn sweep_group(child: &Child) {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let group = Pid::from_raw(child.id() as i32);
    let deadline = Instant::now() + GROUP_SWEEP_LIMIT;
    let mut pause = Duration::from_millis(1);
    while killpg(group, Signal::SIGKILL) != Err(Errno::ESRCH) && Instant::now() < deadline {
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_budget(timeout: Duration) -> CommandBudget {
        CommandBudget {
            timeout,
            cancel: CancellationToken::new(),
        }
    }

    /// Whether `pid` still names a live (non-zombie) process.
    fn is_running(pid: i32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    /// Shell that records its own pid in `pid_file` atomically. A shell
    /// creates a redirection's target before writing to it, so writing the
    /// file in place would let a reader see it empty (#461).
    fn record_pid(pid_file: &std::path::Path) -> String {
        format!(
            "echo $$ > {pid}.tmp && mv {pid}.tmp {pid}",
            pid = pid_file.display()
        )
    }

    /// The pid in `path`, once the file exists and holds a whole pid.
    fn read_pid(path: &std::path::Path) -> Option<i32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Wait (bounded) for `path` to hold a whole pid.
    fn wait_for_pid(path: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = read_pid(path) {
                return pid;
            }
            assert!(Instant::now() < deadline, "no pid in {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_quick_command_returns_its_output_and_status() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo out; echo err >&2; exit 3"]);
        let output = run_bounded(command, &short_budget(Duration::from_secs(10)), "sh").unwrap();
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn a_child_that_never_exits_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        command.args(["-c", &format!("{}; exec sleep 300", record_pid(&pid_file))]);

        let started = Instant::now();
        let result = run_bounded(command, &short_budget(Duration::from_millis(300)), "hang");

        assert!(started.elapsed() < Duration::from_secs(5));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"), "got: {error}");
        // On a loaded host the deadline can fire before the shell records
        // its pid at all; then there is nothing left to check.
        if let Some(pid) = read_pid(&pid_file) {
            assert!(wait_until_gone(pid), "the hung child is still running");
        }
    }

    /// A child that exits but leaves a descendant holding its stdout used
    /// to block `output()` until the descendant finished. The descendant
    /// must be killed and the call must return.
    #[test]
    fn a_descendant_left_behind_is_killed_and_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        // The parent waits until the descendant has recorded its pid, so
        // the descendant is running (not merely forked) when the parent
        // exits, however slow the host.
        command.args([
            "-c",
            &format!(
                "sh -c '{}; exec sleep 300' & while [ ! -e {} ]; do sleep 0.01; done; echo done",
                record_pid(&pid_file),
                pid_file.display()
            ),
        ]);

        let started = Instant::now();
        let output = run_bounded(command, &short_budget(Duration::from_secs(30)), "leaky").unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(output.stdout, b"done\n");
        let pid = read_pid(&pid_file).expect("the parent waited for the pid file");
        assert!(wait_until_gone(pid), "the descendant outlived the call");
    }

    /// Killing the group once can miss a process that was being forked at
    /// that moment, which then outlives the call. When `run_bounded`
    /// returns, nothing may be left in the child's process group.
    #[test]
    fn no_member_of_the_group_is_left_when_the_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let budget = short_budget(Duration::from_secs(300));
        let cancel = budget.cancel.clone();
        let mut command = Command::new("sh");
        // Keep forking short-lived subshells that each start a sleeper, so
        // the group changes while it is being killed.
        command.args([
            "-c",
            &format!(
                "{}; while :; do (sleep 300 &) ; sleep 0.005; done",
                record_pid(&pid_file)
            ),
        ]);

        let call = std::thread::spawn(move || run_bounded(command, &budget, "forker"));
        let group = wait_for_pid(&pid_file);
        cancel.cancel();
        let result = call.join().unwrap();

        assert!(result.unwrap_err().to_string().contains("cancelled"));
        let probe = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), None);
        assert_eq!(
            probe,
            Err(nix::errno::Errno::ESRCH),
            "the child's process group still has members"
        );
    }

    #[test]
    fn cancellation_stops_a_running_child_promptly() {
        let budget = short_budget(Duration::from_secs(300));
        let cancel = budget.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.cancel();
        });
        let mut command = Command::new("sleep");
        command.arg("300");

        let started = Instant::now();
        let result = run_bounded(command, &budget, "sleep");

        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn runaway_output_is_capped() {
        let mut command = Command::new("sh");
        command.args(["-c", "yes"]);
        let result = run_bounded(command, &short_budget(Duration::from_secs(60)), "yes");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("more than"), "got: {error}");
    }

    /// The child has no controlling terminal, so nothing it runs can
    /// prompt on one.
    #[test]
    fn the_child_has_no_controlling_terminal() {
        let mut command = Command::new("sh");
        // The probe runs in a subshell: a failed redirection on a special
        // builtin ends a POSIX shell (dash does), so only the subshell dies.
        command.args([
            "-c",
            "if (: </dev/tty) 2>/dev/null; then echo has-tty; else echo no-tty; fi",
        ]);
        let output = run_bounded(command, &short_budget(Duration::from_secs(10)), "tty").unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "no-tty");
    }
}

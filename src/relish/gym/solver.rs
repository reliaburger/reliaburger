//! Run the user's agent: any program, with a deadline it can't outlive.
//!
//! The solver runs in its own process group, so a timeout or Ctrl-C stops
//! it and everything it started (an agent CLI typically starts a model
//! client, a shell and `relish` calls of its own). [`GroupGuard`] kills the
//! group when it's dropped, so even a run the runner abandons halfway (its
//! future dropped by `tokio::select!` when Ctrl-C wins) leaves no solver
//! behind.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// How much of the solver's own stdout and stderr the gym keeps.
const KEPT_OUTPUT: usize = 256 * 1024;

/// How long a solver gets to exit after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// What a solver did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Attempt {
    /// Its exit code, when it exited.
    pub exit_code: Option<i32>,
    /// The signal that ended it, when one did (a crash, or the gym's kill).
    pub signal: Option<i32>,
    /// How long it ran, in milliseconds.
    pub millis: u64,
    /// Whether the gym stopped it at the deadline.
    pub timed_out: bool,
    /// The start of what it printed.
    #[serde(skip)]
    pub stdout: String,
    /// The start of what it printed to stderr.
    #[serde(skip)]
    pub stderr: String,
}

/// Why a solver didn't produce an attempt.
#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    /// It couldn't start.
    #[error("couldn't start the solver `{program}`: {source}")]
    Spawn {
        /// The program.
        program: String,
        /// Why.
        source: std::io::Error,
    },
    /// The run was cancelled (Ctrl-C) while it ran; it has been stopped.
    #[error("cancelled")]
    Cancelled,
    /// Waiting on it failed.
    #[error("waiting for the solver: {0}")]
    Wait(std::io::Error),
}

/// Kills a process group when dropped, unless disarmed.
struct GroupGuard {
    group: Option<i32>,
}

impl GroupGuard {
    fn signal(&self, signal: i32) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            // SAFETY: killpg takes plain integers and only sends a signal;
            // the group id is the solver's own pid, which `process_group(0)`
            // made a group leader. A group that has already gone gives ESRCH,
            // which is fine to ignore.
            unsafe {
                libc::killpg(group, signal);
            }
        }
        #[cfg(not(unix))]
        let _ = signal;
    }

    fn disarm(&mut self) {
        self.group = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        self.signal(libc::SIGKILL);
    }
}

async fn read_capped(mut reader: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = KEPT_OUTPUT.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Run `argv` as the solver in `work_dir`, with exactly `env` as its
/// environment and `prompt` on stdin, for at most `timeout`.
///
/// It returns when the solver exits or is stopped at the deadline. If
/// `cancel` fires first, the solver's whole process group is killed and the
/// answer is [`SolverError::Cancelled`].
pub async fn run(
    argv: &[String],
    prompt: &str,
    env: &BTreeMap<String, String>,
    work_dir: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Attempt, SolverError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(SolverError::Spawn {
            program: String::new(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no program"),
        });
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| SolverError::Spawn {
        program: program.clone(),
        source,
    })?;
    let mut guard = GroupGuard {
        group: child.id().and_then(|pid| i32::try_from(pid).ok()),
    };

    let stdin = child.stdin.take();
    let prompt = prompt.to_string();
    // A solver that never reads stdin mustn't block the gym, so the prompt
    // goes in from its own task.
    tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
    });
    let stdout = child
        .stdout
        .take()
        .map(|out| tokio::spawn(read_capped(out)));
    let stderr = child
        .stderr
        .take()
        .map(|err| tokio::spawn(read_capped(err)));

    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status.map_err(SolverError::Wait)?,
        () = tokio::time::sleep(timeout) => {
            timed_out = true;
            #[cfg(unix)]
            guard.signal(libc::SIGTERM);
            match tokio::time::timeout(TERM_GRACE, child.wait()).await {
                Ok(status) => status.map_err(SolverError::Wait)?,
                Err(_) => {
                    #[cfg(unix)]
                    guard.signal(libc::SIGKILL);
                    child.wait().await.map_err(SolverError::Wait)?
                }
            }
        }
        () = cancel.cancelled() => {
            // The guard's drop kills the group.
            return Err(SolverError::Cancelled);
        }
    };
    // The solver has gone, but something it started may still hold the
    // group (a background `relish`): stop it before the gym cleans up.
    #[cfg(unix)]
    guard.signal(libc::SIGKILL);
    guard.disarm();

    let collect = |task: Option<tokio::task::JoinHandle<String>>| async move {
        match task {
            Some(task) => tokio::time::timeout(TERM_GRACE, task)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default(),
            None => String::new(),
        }
    };
    let stdout = collect(stdout).await;
    let stderr = collect(stderr).await;
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    Ok(Attempt {
        exit_code: status.code(),
        signal,
        millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        timed_out,
        stdout,
        stderr,
    })
}

/// Parse `--solver`: `baseline`, or `command:PROGRAM ARGS...`.
///
/// `baseline` runs this `relish` as `relish gym baseline`, so the built-in
/// playbook goes through exactly the path any other solver does.
pub fn parse_spec(spec: &str, relish: &Path) -> Result<Vec<String>, String> {
    if spec == "baseline" {
        return Ok(vec![
            relish.display().to_string(),
            "gym".to_string(),
            "baseline".to_string(),
        ]);
    }
    let Some(command) = spec.strip_prefix("command:") else {
        return Err(format!(
            "unknown solver `{spec}`: use baseline, or command:PROGRAM ARGS"
        ));
    };
    let mut argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    let Some(program) = argv.first_mut() else {
        return Err("command: needs a program".to_string());
    };
    // The solver runs in its own directory, so a relative path to the
    // program means relative to where the gym was started.
    if program.contains('/') && !Path::new(program.as_str()).is_absolute() {
        let absolute = std::env::current_dir()
            .map(|dir| dir.join(&*program))
            .map_err(|error| format!("can't resolve {program}: {error}"))?;
        *program = absolute.display().to_string();
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())])
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[tokio::test]
    async fn a_solver_reads_the_prompt_and_its_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = run(
            &sh("cat; echo done >&2"),
            "the prompt",
            &env(),
            dir.path(),
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(attempt.exit_code, Some(0));
        assert_eq!(attempt.stdout, "the prompt");
        assert_eq!(attempt.stderr, "done\n");
        assert!(!attempt.timed_out);
    }

    #[tokio::test]
    async fn the_solver_sees_only_the_environment_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = run(
            &sh("echo \"${HOME:-unset} ${GYM_ANSWER}\""),
            "",
            &BTreeMap::from([("GYM_ANSWER".to_string(), "a.json".to_string())]),
            dir.path(),
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(attempt.stdout, "unset a.json\n");
    }

    #[tokio::test]
    async fn a_crashing_solver_is_an_attempt_with_its_signal() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = run(
            &sh("kill -SEGV $$"),
            "",
            &env(),
            dir.path(),
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(attempt.exit_code, None);
        assert_eq!(attempt.signal, Some(libc::SIGSEGV));
    }

    #[tokio::test]
    async fn a_solver_past_its_deadline_is_stopped_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        // The solver starts a grandchild that would outlive a plain kill of
        // the solver itself.
        let script = format!("sleep 60 & echo $! > {}; wait", marker.display());
        let attempt = run(
            &sh(&script),
            "",
            &env(),
            dir.path(),
            Duration::from_millis(300),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(attempt.timed_out);
        let pid: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid).await, "the grandchild {pid} outlived the timeout");
    }

    #[tokio::test]
    async fn cancelling_stops_the_solver_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        let script = format!("sleep 60 & echo $! > {}; wait", marker.display());
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let error = run(
            &sh(&script),
            "",
            &env(),
            dir.path(),
            Duration::from_secs(60),
            &cancel,
        )
        .await
        .unwrap_err();
        canceller.await.unwrap();
        assert!(matches!(error, SolverError::Cancelled), "{error}");
        let pid: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid).await, "the grandchild {pid} outlived the cancel");
    }

    /// Whether `pid` has exited, allowing it a moment to be reaped.
    async fn gone(pid: i32) -> bool {
        for _ in 0..50 {
            // SAFETY: signal 0 only checks the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[test]
    fn the_solver_spec_is_baseline_or_a_command() {
        let relish = Path::new("/usr/local/bin/relish");
        assert_eq!(
            parse_spec("baseline", relish).unwrap(),
            ["/usr/local/bin/relish", "gym", "baseline"]
        );
        assert_eq!(
            parse_spec("command:/opt/agent --fast", relish).unwrap(),
            ["/opt/agent", "--fast"]
        );
        let relative = parse_spec("command:./agent.sh", relish).unwrap();
        assert!(Path::new(&relative[0]).is_absolute(), "{relative:?}");
        assert!(parse_spec("command:", relish).is_err());
        assert!(parse_spec("scripted", relish).is_err());
    }
}

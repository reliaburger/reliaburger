//! Inspect a verified executable before it can replace the running process.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::UpgradeError;

/// How long a candidate has, from launch, to answer `--compatibility`.
///
/// This is a hang guard, not a speed target: a candidate that answers takes
/// milliseconds. The allowance is wide because launch is not all the
/// candidate's own time. macOS assesses the first exec of every new
/// executable file before it runs, one file at a time across the whole host,
/// and the private copy below is always new. Outside a developer-tools
/// context that costs about 0.13 s for a small file and seconds for a large
/// one, and a probe waits behind every other new executable the host is
/// starting (#722).
pub(crate) const QUERY_DEADLINE: Duration = Duration::from_secs(60);

/// The largest answer a candidate may give.
const ANSWER_LIMIT: usize = 4096;

/// How far a compatibility query got, so a failure says which phase stalled.
#[derive(Debug, Default)]
struct QueryProgress {
    /// The candidate's process id once it was launched.
    pid: Option<u32>,
    /// From the start of the deadline to a successful launch.
    launched: Option<Duration>,
    /// From the start of the deadline to the candidate's first output.
    first_output: Option<Duration>,
    /// From the start of the deadline to the candidate's exit.
    exited: Option<Duration>,
}

impl std::fmt::Display for QueryProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.pid, self.launched) {
            (Some(pid), Some(after)) => write!(f, "launched as pid {pid} after {after:?}")?,
            _ => write!(f, "not launched")?,
        }
        match self.first_output {
            Some(after) => write!(f, ", first output after {after:?}")?,
            None => write!(f, ", no output")?,
        }
        match self.exited {
            Some(after) => write!(f, ", exited after {after:?}"),
            None => write!(f, ", still running"),
        }
    }
}

/// A private, executable copy of a candidate binary, written for one
/// compatibility query. Dropping it deletes the file.
pub(super) struct Probe(tempfile::TempPath);

/// Write a private copy of verified bytes that [`query_probe`] can run.
///
/// This writes the whole binary, so it blocks: call it on the blocking
/// pool, next to the signature check that read the same bytes (#794). A
/// private copy keeps the probe independent of a mutable download path.
pub(super) fn write_probe(bytes: &[u8], directory: &Path) -> Result<Probe, UpgradeError> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    // Close the writable file before exec (Linux refuses ETXTBSY otherwise).
    Ok(Probe(file.into_temp_path()))
}

/// Run a probe whose bytes were signature-checked before it was written,
/// and check that it reads and writes this build's formats.
///
/// The probe is deleted afterwards on the blocking pool: unlinking a
/// release-sized file frees all of its pages, which on a busy disk took
/// longer than a Raft heartbeat on the caller's worker (#794).
pub(super) async fn query_probe(probe: Probe) -> Result<(), UpgradeError> {
    let answer = query(&probe.0).await;
    // A delete that fails leaves a stray temporary file behind, as
    // `TempPath`'s own drop would; the answer is what the caller needs.
    let _ = tokio::task::spawn_blocking(move || probe.0.close()).await;
    answer
}

/// Ask the executable at `executable` which formats it uses.
async fn query(executable: &Path) -> Result<(), UpgradeError> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--compatibility")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    query_with(command).await
}

/// What [`launch`] hands back: the command, for a retry, and its result.
type Launched = (
    tokio::process::Command,
    std::io::Result<tokio::process::Child>,
);

/// Start `command` on the blocking pool and hand it back with the result.
///
/// Launching blocks until the new program has been exec'd, and on macOS
/// the first exec of a new file waits for the host to assess it: seconds,
/// for a release-sized binary on a busy host. That wait would stop every
/// task on the caller's worker, the node's Raft core among them (#793).
/// The command comes back so an `ETXTBSY` retry can launch it again.
async fn launch(mut command: tokio::process::Command) -> Result<Launched, UpgradeError> {
    // Blocking-pool threads carry the runtime's context, so the child is
    // registered with the runtime that will wait for it. If the caller is
    // cancelled first, the child is dropped when the launch finishes, and
    // `kill_on_drop` stops it.
    let launched = tokio::task::spawn_blocking(move || {
        let launched = command.spawn();
        (command, launched)
    })
    .await
    .map_err(std::io::Error::other)?;
    Ok(launched)
}

/// Run a prepared compatibility query and check the answer.
async fn query_with(mut command: tokio::process::Command) -> Result<(), UpgradeError> {
    let started = tokio::time::Instant::now();
    let deadline = started + QUERY_DEADLINE;
    let mut progress = QueryProgress::default();
    let mut child = loop {
        let (returned, launched) = launch(command).await?;
        command = returned;
        match launched {
            Ok(child) => break child,
            // Another concurrent fork can briefly inherit the writable file
            // before its exec closes CLOEXEC descriptors. Retain the same
            // private verified bytes and the original query deadline.
            Err(error)
                if error.raw_os_error() == Some(nix::libc::ETXTBSY)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + Duration::from_millis(10)).min(deadline),
                )
                .await;
            }
            Err(error) => {
                return Err(UpgradeError::IncompatibleBinary(format!(
                    "cannot query candidate: {error}"
                )));
            }
        }
    };
    progress.pid = child.id();
    progress.launched = Some(started.elapsed());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpgradeError::IncompatibleBinary("missing query output pipe".into()))?;

    // Wait for both the end of the answer and the exit, noting when each
    // happens. `read` and `wait` are both cancel-safe, so losing a `select!`
    // round to the other branch drops nothing.
    let mut output = Vec::new();
    let mut status = None;
    let mut answered = false;
    let mut chunk = [0u8; 1024];
    let expired = tokio::time::sleep_until(deadline);
    tokio::pin!(expired);
    let outcome = loop {
        if let (true, Some(status)) = (answered, status) {
            break Ok(status);
        }
        tokio::select! {
            read = stdout.read(&mut chunk), if !answered => match read {
                Ok(0) => answered = true,
                Ok(n) => {
                    progress.first_output.get_or_insert(started.elapsed());
                    output.extend_from_slice(&chunk[..n]);
                    if output.len() > ANSWER_LIMIT {
                        break Err(format!("output exceeds {ANSWER_LIMIT} bytes"));
                    }
                }
                Err(error) => break Err(format!("cannot read the answer: {error}")),
            },
            exit = child.wait(), if status.is_none() => match exit {
                Ok(exit) => {
                    progress.exited = Some(started.elapsed());
                    status = Some(exit);
                }
                Err(error) => break Err(format!("cannot wait for the candidate: {error}")),
            },
            () = &mut expired => {
                break Err(format!("timed out after {}s", QUERY_DEADLINE.as_secs()));
            }
        }
    };
    let status = match outcome {
        Ok(status) => status,
        Err(reason) => {
            // Reap before the caller deletes the temporary executable;
            // cancellation also kills the child through Command::kill_on_drop.
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(UpgradeError::IncompatibleBinary(format!(
                "compatibility query {reason} ({progress})"
            )));
        }
    };
    if !status.success() {
        return Err(UpgradeError::IncompatibleBinary(format!(
            "compatibility query exited {status} ({progress})"
        )));
    }
    let formats: crate::compatibility::Compatibility =
        serde_json::from_slice(&output).map_err(|e| {
            UpgradeError::IncompatibleBinary(format!("invalid compatibility response: {e}"))
        })?;
    if formats == crate::compatibility::CURRENT {
        Ok(())
    } else {
        Err(UpgradeError::IncompatibleFormats {
            found: formats,
            expected: crate::compatibility::CURRENT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_probe_launch_leaves_the_runtime_free() {
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;
        let directory = tempfile::tempdir().unwrap();
        let formats = serde_json::to_string(&crate::compatibility::CURRENT).unwrap();
        let script = format!("#!/bin/sh\nprintf '%s' '{formats}'\nexit 0\n");
        let probe = write_probe(script.as_bytes(), directory.path()).unwrap();
        let mut command = tokio::process::Command::new(&probe.0);
        command
            .arg("--compatibility")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Stand in for macOS's first-launch assessment (#793), which holds
        // the parent's spawn until the kernel has finished with the new
        // file. A sleep between fork and exec holds it the same way, on any
        // host. (A slow script can't: `spawn` returns once exec succeeds,
        // before the script's first line runs.)
        // SAFETY: the closure runs in the forked child before exec, where
        // only async-signal-safe calls are allowed. `thread::sleep` is a
        // loop around nanosleep, which is one, and it neither allocates nor
        // takes a lock.
        unsafe {
            command.pre_exec(|| {
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            });
        }

        let stop = CancellationToken::new();
        let ticker = tokio::spawn({
            let stop = stop.clone();
            async move {
                let mut longest = Duration::ZERO;
                let mut last = std::time::Instant::now();
                while !stop.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    longest = longest.max(last.elapsed());
                    last = std::time::Instant::now();
                }
                longest
            }
        });
        // Let the ticker take its first reading: on this one-thread runtime
        // it doesn't run until the test yields, and a launch that blocks
        // before its first `.await` would otherwise go unseen.
        tokio::task::yield_now().await;
        let started = std::time::Instant::now();
        let answered = query_with(command).await;
        let took = started.elapsed();
        stop.cancel();
        let longest = ticker.await.unwrap();

        answered.unwrap();
        assert!(
            took >= Duration::from_secs(1),
            "the launch took only {took:?}"
        );
        // The same bound as `preparing_a_large_binary_leaves_the_runtime_free`:
        // a quarter of Raft's shortest election timeout.
        assert!(
            longest < Duration::from_millis(250),
            "the probe launch held the runtime for {longest:?} of its {took:?}"
        );
    }
}

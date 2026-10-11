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

/// Run only bytes whose release signature has already been verified by the caller.
/// A private copy keeps the probe independent of a mutable download path.
pub(super) async fn check_binary(bytes: Vec<u8>, directory: &Path) -> Result<(), UpgradeError> {
    let directory = directory.to_path_buf();
    let executable = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&bytes)?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        // Close the writable file before exec (Linux refuses ETXTBSY otherwise).
        Ok(file.into_temp_path())
    })
    .await
    .map_err(|e| UpgradeError::IncompatibleBinary(e.to_string()))??;
    let started = tokio::time::Instant::now();
    let deadline = started + QUERY_DEADLINE;
    let mut progress = QueryProgress::default();
    let mut child = loop {
        match tokio::process::Command::new(&executable)
            .arg("--compatibility")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
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
            // Reap before dropping the temporary executable; cancellation also
            // kills the child through Command::kill_on_drop.
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

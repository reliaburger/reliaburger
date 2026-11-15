//! The `relish` a solver runs: this binary, recording every call.
//!
//! The gym puts a two-line `relish` script first on the solver's `PATH`. It
//! runs `relish gym record --log LOG -- ARGS`, which runs this same binary
//! with `ARGS`, passes its output and exit code through, and appends one JSON
//! line to the log: the arguments, the exit code, the output and how long it
//! took. Scoring reads that log, not the solver's own account of what it
//! did, so any agent is judged the same way.
//!
//! The gym's Python shim records each `relish mcp` tool call by reading the
//! JSON-RPC stream; this one passes `relish mcp` straight through and
//! records it as one call. MCP tools are read-only by construction, and the
//! audit log still shows anything the token tried.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::score::Call;

/// Enough for any `relish agent` answer and the start of anything longer.
const MAX_KEPT_BYTES: usize = 256 * 1024;

fn kept(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_KEPT_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_KEPT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[gym: output cut]", &text[..end])
}

/// Quote `text` for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Write the recording `relish` into `bin_dir`, running `relish_exe` and
/// logging to `log`.
pub fn install(bin_dir: &Path, relish_exe: &Path, log: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(bin_dir)?;
    let script = bin_dir.join("relish");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {} gym record --log {} -- \"$@\"\n",
            shell_quote(&relish_exe.display().to_string()),
            shell_quote(&log.display().to_string()),
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(script)
}

fn append(log: &Path, call: &Call) -> std::io::Result<()> {
    let mut line = serde_json::to_string(call).map_err(std::io::Error::other)?;
    line.push('\n');
    // One write to an append-only file, so two calls a solver runs at once
    // don't interleave their lines.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?
        .write_all(line.as_bytes())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Whether a `relish mcp` call is the server (streamed, not captured).
fn is_mcp_server(args: &[String]) -> bool {
    args.first().is_some_and(|word| word == "mcp")
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--list"))
}

/// Run `relish_exe` with `args`, pass its output through, log the call and
/// return its exit code (128 plus the signal when a signal ended it).
pub async fn record(relish_exe: &Path, log: &Path, args: &[String]) -> std::io::Result<i32> {
    let started = now();
    let clock = Instant::now();
    let mut command = tokio::process::Command::new(relish_exe);
    command.args(args).stdin(Stdio::inherit());
    let mcp = is_mcp_server(args);
    let (status, stdout, stderr) = if mcp {
        let status = command
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await?;
        (status, Vec::new(), Vec::new())
    } else {
        let output = command.output().await?;
        (output.status, output.stdout, output.stderr)
    };
    {
        let mut out = std::io::stdout().lock();
        out.write_all(&stdout)?;
        out.flush()?;
        let mut err = std::io::stderr().lock();
        err.write_all(&stderr)?;
        err.flush()?;
    }
    let call = Call {
        argv: std::iter::once("relish".to_string())
            .chain(args.iter().cloned())
            .collect(),
        exit: status.code(),
        stdout: kept(&stdout),
        stderr: kept(&stderr),
        started: Some(started),
        seconds: Some((clock.elapsed().as_secs_f64() * 1000.0).round() / 1000.0),
        via: mcp.then(|| "passthrough".to_string()),
    };
    append(log, &call)?;
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    Ok(status.code().unwrap_or_else(|| 128 + signal.unwrap_or(0)))
}

/// Every call in a log, in order; an absent log is no calls, and a line that
/// isn't a call (a solver that wrote to the log itself) is skipped.
pub fn read_log(log: &Path) -> Vec<Call> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_quotes_paths_with_spaces_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let script = install(
            &dir.path().join("bin"),
            Path::new("/opt/it's here/relish"),
            Path::new("/tmp/calls log.jsonl"),
        )
        .unwrap();
        let text = std::fs::read_to_string(script).unwrap();
        assert_eq!(
            text,
            "#!/bin/sh\nexec '/opt/it'\\''s here/relish' gym record --log '/tmp/calls log.jsonl' -- \"$@\"\n"
        );
    }

    #[tokio::test]
    async fn a_call_is_passed_through_and_logged() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-relish");
        std::fs::write(&fake, "#!/bin/sh\necho \"out $*\"\necho err >&2\nexit 3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = dir.path().join("calls.jsonl");
        let args = vec!["delete".to_string(), "gym-web".to_string()];
        let code = record(&fake, &log, &args).await.unwrap();
        assert_eq!(code, 3);
        let calls = read_log(&log);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv, ["relish", "delete", "gym-web"]);
        assert_eq!(calls[0].exit, Some(3));
        assert_eq!(calls[0].stdout, "out delete gym-web\n");
        assert_eq!(calls[0].stderr, "err\n");
        assert_eq!(calls[0].via, None);
    }

    #[test]
    fn a_long_output_is_cut_and_says_so() {
        let text = kept(&vec![b'x'; MAX_KEPT_BYTES + 10]);
        assert!(text.ends_with("[gym: output cut]"));
        assert_eq!(kept(b"short"), "short");
    }

    #[test]
    fn a_line_that_isnt_a_call_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.jsonl");
        std::fs::write(
            &log,
            "{\"argv\":[\"relish\",\"status\"],\"exit\":0}\nnot json\n\n",
        )
        .unwrap();
        assert_eq!(read_log(&log).len(), 1);
        assert!(read_log(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn only_the_mcp_server_streams() {
        let words = |line: &str| {
            line.split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>()
        };
        assert!(is_mcp_server(&words("mcp")));
        assert!(is_mcp_server(&words("mcp --no-logs")));
        assert!(!is_mcp_server(&words("mcp --list")));
        assert!(!is_mcp_server(&words("status")));
    }
}

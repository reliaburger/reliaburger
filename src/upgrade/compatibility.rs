//! Inspect a verified executable before it can replace the running process.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::UpgradeError;

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpgradeError::IncompatibleBinary("missing query output pipe".into()))?;
    let result = tokio::time::timeout_at(deadline, async {
        let read = async {
            let mut output = Vec::new();
            stdout.take(4097).read_to_end(&mut output).await?;
            if output.len() > 4096 {
                return Err(std::io::Error::other(
                    "compatibility output exceeds 4096 bytes",
                ));
            }
            Ok(output)
        };
        tokio::try_join!(child.wait(), read)
    })
    .await;
    let (status, output) = match result {
        Ok(Ok(result)) => result,
        error => {
            // Reap before dropping the temporary executable; cancellation also
            // kills the child through Command::kill_on_drop.
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(UpgradeError::IncompatibleBinary(format!(
                "compatibility query failed: {error:?}"
            )));
        }
    };
    if !status.success() {
        return Err(UpgradeError::IncompatibleBinary(format!(
            "compatibility query exited {status}"
        )));
    }
    let formats: crate::compatibility::Compatibility =
        serde_json::from_slice(&output).map_err(|e| {
            UpgradeError::IncompatibleBinary(format!("invalid compatibility response: {e}"))
        })?;
    formats
        .require_current()
        .map_err(|e| UpgradeError::IncompatibleBinary(e.to_string()))
}

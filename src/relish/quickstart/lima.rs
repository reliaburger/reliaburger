//! Deadline-bound Lima commands and private guest file transfer.

use anyhow::{Context, Result, bail};
use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

/// Runs a selected Lima binary; command errors never include its argument list.
#[derive(Clone)]
pub struct Lima {
    executable: PathBuf,
    timeout: Duration,
}

impl Lima {
    /// Use an explicit executable and a deadline for each child process.
    pub fn new(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
        }
    }

    /// Execute a command, killing its direct child if the deadline expires.
    pub async fn command(&self, args: &[&str]) -> Result<String> {
        let mut command = tokio::process::Command::new(&self.executable);
        command
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null());
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .context("Lima command exceeded its deadline")?
            .context("failed to execute Lima")?;
        if !output.status.success() {
            bail!(
                "Lima command failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8(output.stdout).context("Lima returned invalid UTF-8")
    }

    /// Read the status of exactly one owned VM; absence is not a command failure.
    pub async fn status(&self, name: &str) -> Result<Option<String>> {
        let output = self.command(&["list", "--json"]).await?;
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)?;
            if value["name"].as_str() == Some(name) {
                return Ok(Some(
                    value["status"]
                        .as_str()
                        .context("missing VM status")?
                        .to_owned(),
                ));
            }
        }
        Ok(None)
    }

    /// Get the peer-reachable address; never fall back to Lima's isolated NAT IP.
    pub async fn address(&self, name: &str) -> Result<Ipv4Addr> {
        let output = self
            .command(&["shell", name, "ip", "-4", "-o", "addr", "show", "lima0"])
            .await?;
        let address = output
            .split_whitespace()
            .skip_while(|part| *part != "inet")
            .nth(1)
            .and_then(|cidr| cidr.split('/').next())
            .context("VM has no shared-network IPv4 address")?;
        let address: Ipv4Addr = address.parse()?;
        if !address.is_private() || address.is_loopback() {
            bail!("VM shared-network address is not private");
        }
        Ok(address)
    }

    /// Transfer through a private guest directory and remove it even on failure.
    pub async fn install(
        &self,
        vm: &str,
        source: &Path,
        destination: &str,
        executable: bool,
    ) -> Result<()> {
        let staging = self
            .command(&["shell", vm, "mktemp", "-d", "/tmp/rb-install.XXXXXXXXXX"])
            .await?;
        let staging = staging.trim();
        if !staging.starts_with("/tmp/rb-install.")
            || !staging
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/.-".contains(&b))
        {
            bail!("guest returned an invalid private staging path");
        }
        let result: Result<()> = async {
            let remote = format!("{staging}/payload");
            self.command(&[
                "copy",
                source.to_str().context("non-UTF-8 source path")?,
                &format!("{vm}:{remote}"),
            ])
            .await?;
            let replacement = format!("{destination}.rb-staging");
            self.command(&[
                "shell",
                vm,
                "sudo",
                "install",
                "-D",
                "-m",
                if executable { "755" } else { "600" },
                &remote,
                &replacement,
            ])
            .await?;
            // Renaming also works if a previous attempt already started this executable.
            self.command(&[
                "shell",
                vm,
                "sudo",
                "mv",
                "-f",
                "--",
                &replacement,
                destination,
            ])
            .await?;
            Ok(())
        }
        .await;
        let cleanup = self
            .command(&["shell", vm, "rm", "-rf", "--", staging])
            .await;
        result?;
        cleanup?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_timeout_is_bounded_and_does_not_print_arguments() {
        let lima = Lima::new("/bin/sh".into(), Duration::from_millis(50));
        let error = lima
            .command(&["-c", "sleep 5", "sensitive-value"])
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("sensitive-value"));
        assert!(error.to_string().contains("deadline"));
    }

    #[tokio::test]
    async fn command_failure_is_reported_instead_of_empty_success() {
        let lima = Lima::new("/bin/sh".into(), Duration::from_secs(2));
        assert!(
            lima.command(&["-c", "exit 17"])
                .await
                .unwrap_err()
                .to_string()
                .contains("17")
        );
        assert_eq!(
            lima.command(&["-c", "printf ready"]).await.unwrap(),
            "ready"
        );
    }
}

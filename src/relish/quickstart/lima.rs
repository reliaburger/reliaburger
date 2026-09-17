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
    home: Option<PathBuf>,
}

impl Lima {
    /// Use an explicit executable and a deadline for each child process.
    pub fn new(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
            home: None,
        }
    }

    /// Isolate managed VM metadata, networking and SSH keys from global Lima settings.
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Execute a command, killing its direct child if the deadline expires.
    pub async fn command(&self, args: &[&str]) -> Result<String> {
        let mut command = tokio::process::Command::new(&self.executable);
        if let Some(home) = &self.home {
            command.env("LIMA_HOME", home);
        }
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
        // This asks the kernel for a route; it sends no packet to this address.
        let output = self
            .command(&["shell", name, "ip", "-j", "-4", "route", "get", "1.1.1.1"])
            .await?;
        shared_address(&output)
    }

    /// Wait for provisioning after an interrupted start left a running but unfinished VM.
    pub async fn wait_for_guest(&self, name: &str) -> Result<()> {
        tokio::time::timeout(self.timeout, async {
            loop {
                if self.command(&["shell", name, "sudo", "sh", "-c",
                    "test -f /run/lima-boot-done && command -v runc >/dev/null && command -v btrfs >/dev/null && command -v nft >/dev/null"])
                    .await.is_ok() { return; }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }).await.context("guest provisioning did not finish before its deadline")?;
        Ok(())
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

fn shared_address(route: &str) -> Result<Ipv4Addr> {
    let routes: Vec<serde_json::Value> = serde_json::from_str(route)?;
    if routes.len() != 1 {
        bail!("guest has no unambiguous default route");
    }
    let address: Ipv4Addr = routes[0]["prefsrc"]
        .as_str()
        .context("guest route has no source IPv4 address")?
        .parse()?;
    if !address.is_private() || address.octets()[..3] == [192, 168, 5] {
        bail!("guest default route is not on the shared private network");
    }
    Ok(address)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shared_address_uses_route_source_instead_of_an_interface_name() {
        let route = r#"[{"dev":"eth0","gateway":"192.168.104.2","prefsrc":"192.168.104.7"}]"#;
        assert_eq!(
            shared_address(route).unwrap(),
            "192.168.104.7".parse::<Ipv4Addr>().unwrap()
        );
        for route in [
            r#"[{"prefsrc":"192.168.5.15"}]"#,
            r#"[{"prefsrc":"127.0.0.1"}]"#,
            r#"[{"prefsrc":"203.0.113.1"}]"#,
            "[]",
        ] {
            assert!(shared_address(route).is_err());
        }
    }

    #[tokio::test]
    async fn managed_lima_uses_its_own_home_instead_of_user_overrides() {
        let lima = Lima::new("/bin/sh".into(), Duration::from_secs(2))
            .with_home("/private/managed-lima".into());
        assert_eq!(
            lima.command(&["-c", "printf '%s' \"$LIMA_HOME\""])
                .await
                .unwrap(),
            "/private/managed-lima"
        );
    }

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

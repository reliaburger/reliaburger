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

    /// Create Lima's shared SSH key before any VM starts.
    ///
    /// Lima 2.1.0 generates `$LIMA_HOME/_config/user` on first start, but it
    /// checks for the key before taking its lock and never checks again, so
    /// VMs created concurrently can overwrite each other's key. We generate
    /// the same ed25519 key it would, publishing the public half first because
    /// Lima treats the private file as proof that both exist.
    pub async fn ensure_user_key(&self) -> Result<()> {
        let home = self
            .home
            .as_ref()
            .context("managed Lima has no private home")?;
        let config = home.join("_config");
        let private = config.join("user");
        if tokio::fs::try_exists(&private).await? {
            return Ok(());
        }
        let staging = {
            let config = config.clone();
            tokio::task::spawn_blocking(move || -> Result<tempfile::TempDir> {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&config)?;
                Ok(tempfile::Builder::new()
                    .prefix(".user-key-")
                    .tempdir_in(&config)?)
            })
            .await??
        };
        let key = staging.path().join("user");
        let status = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-q", "-N", "", "-C", "lima", "-f"])
                .arg(&key)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await
        .context("ssh-keygen exceeded its deadline")?
        .context("failed to run ssh-keygen; Lima needs OpenSSH on the host")?;
        if !status.success() {
            bail!("ssh-keygen failed ({status})");
        }
        tokio::fs::rename(key.with_extension("pub"), config.join("user.pub")).await?;
        tokio::fs::rename(&key, &private).await?;
        Ok(())
    }

    /// True once Lima's shared `user-v2` network daemon is running.
    ///
    /// The first `limactl start` launches it, again without re-checking under
    /// its lock, so peers wait for it rather than racing to start a second one.
    pub async fn shared_network_running(&self) -> bool {
        let Some(home) = &self.home else {
            return false;
        };
        let network = home.join("_networks/user-v2");
        let Ok(pid) = tokio::fs::read_to_string(network.join("usernet_user-v2.pid")).await else {
            return false;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            return false;
        };
        process_alive(pid)
            && tokio::fs::try_exists(network.join("user-v2_fd.sock"))
                .await
                .unwrap_or(false)
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

fn process_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
        // Signal 0 only checks existence; EPERM still means the process exists.
        pid > 0 && matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
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
    async fn shared_key_is_created_once_in_the_format_lima_expects() {
        let home = tempfile::tempdir().unwrap();
        let lima =
            Lima::new("/bin/sh".into(), Duration::from_secs(2)).with_home(home.path().to_owned());
        lima.ensure_user_key().await.unwrap();
        let config = home.path().join("_config");
        let public = std::fs::read_to_string(config.join("user.pub")).unwrap();
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        assert!(public.trim_end().ends_with(" lima"), "{public}");
        let private = std::fs::read(config.join("user")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&config.join("user")), 0o600);
        assert_eq!(mode(&config), 0o700);
        // Only the key pair is left behind, never a staging directory.
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 2);
        lima.ensure_user_key().await.unwrap();
        assert_eq!(std::fs::read(config.join("user")).unwrap(), private);
    }

    #[tokio::test]
    async fn shared_network_needs_a_live_daemon_and_its_socket() {
        let home = tempfile::tempdir().unwrap();
        let lima =
            Lima::new("/bin/sh".into(), Duration::from_secs(2)).with_home(home.path().to_owned());
        assert!(!lima.shared_network_running().await);
        let network = home.path().join("_networks/user-v2");
        std::fs::create_dir_all(&network).unwrap();
        std::fs::write(network.join("user-v2_fd.sock"), b"").unwrap();
        // A stale PID file from a stopped cluster must not count as running.
        let mut exited = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let stale = exited.id();
        exited.wait().unwrap();
        std::fs::write(network.join("usernet_user-v2.pid"), stale.to_string()).unwrap();
        assert!(!lima.shared_network_running().await);
        std::fs::write(
            network.join("usernet_user-v2.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        assert!(lima.shared_network_running().await);
        std::fs::remove_file(network.join("user-v2_fd.sock")).unwrap();
        assert!(!lima.shared_network_running().await);
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

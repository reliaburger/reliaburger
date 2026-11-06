//! Reaching existing Linux machines over SSH, for `relish setup --ssh`.
//!
//! We drive the system `ssh` binary rather than an SSH library, so a person's
//! keys, agent, `~/.ssh/config` and known hosts all work as they do at a
//! prompt. `BatchMode` means a missing key fails at once instead of waiting
//! for a password nobody will type.

use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::lima::{GuestFile, install_script, write_bundle};

/// One machine to build a cluster on: `[user@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshTarget {
    /// Login user; `ssh` picks one from its own configuration when absent.
    pub user: Option<String>,
    /// Host name or IPv4 address.
    pub host: String,
    /// SSH port.
    pub port: u16,
}

impl SshTarget {
    /// The destination argument for `ssh`.
    fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

impl std::fmt::Display for SshTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.destination())?;
        if self.port != 22 {
            write!(formatter, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// A user or host `ssh` will read as a name, never as an option or a shell word.
fn plain_word(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

impl FromStr for SshTarget {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (user, rest) = match text.split_once('@') {
            Some((user, rest)) => (Some(user), rest),
            None => (None, text),
        };
        let (host, port) = match rest.split_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .with_context(|| format!("{text:?} has an invalid port"))?;
                (host, port)
            }
            None => (rest, 22),
        };
        if port == 0 {
            bail!("{text:?} has an invalid port");
        }
        if !plain_word(host) || user.is_some_and(|user| !plain_word(user)) {
            bail!("{text:?} isn't [user@]host[:port]");
        }
        Ok(Self {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port,
        })
    }
}

/// Parse a comma-separated list of machines, refusing the same one twice.
pub fn parse_targets(list: &str) -> Result<Vec<SshTarget>> {
    let targets = list
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<SshTarget>>>()?;
    let mut seen = HashSet::new();
    for target in &targets {
        if !seen.insert((&target.host, target.port)) {
            bail!("{target} is listed twice");
        }
    }
    Ok(targets)
}

/// Quote one word for the remote shell, which re-parses what `ssh` sends it.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The remote command that runs `script` with `args`, as root or as the login user.
fn remote_command(script: &str, args: &[&str], root: bool) -> String {
    let mut words = vec![
        if root { "sudo -n sh -c" } else { "sh -c" }.to_owned(),
        quote(script),
        "sh".to_owned(),
    ];
    words.extend(args.iter().map(|arg| quote(arg)));
    words.join(" ")
}

/// Runs commands on machines through the system `ssh`.
#[derive(Debug, Clone)]
pub struct Ssh {
    executable: PathBuf,
    script: Option<PathBuf>,
    timeout: Duration,
    root: String,
}

impl Ssh {
    /// Use the `ssh` on `PATH`, with a deadline for each command.
    pub fn new(timeout: Duration) -> Self {
        Self {
            executable: PathBuf::from("ssh"),
            script: None,
            timeout,
            root: String::new(),
        }
    }

    /// A test stand-in for `ssh`, run by `/bin/sh` as `Lima::shell_script` is.
    /// `root` prefixes the paths files are installed to.
    #[cfg(test)]
    pub(crate) fn shell_script(script: PathBuf, timeout: Duration, root: &str) -> Self {
        Self {
            executable: PathBuf::from("/bin/sh"),
            script: Some(script),
            timeout,
            root: root.to_owned(),
        }
    }

    fn command(&self, target: &SshTarget, options: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.executable);
        if let Some(script) = &self.script {
            command.arg(script);
        }
        command
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .args(["-o", "ServerAliveInterval=15"])
            .args(["-p", &target.port.to_string()])
            .args(options)
            .kill_on_drop(true);
        command
    }

    async fn run(
        &self,
        target: &SshTarget,
        remote: String,
        stdin: std::process::Stdio,
    ) -> Result<String> {
        let mut command = self.command(target, &[]);
        command.arg("--").arg(target.destination()).arg(remote);
        command.stdin(stdin);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .with_context(|| format!("{target}: ssh exceeded its deadline"))?
            .context("failed to execute ssh; is OpenSSH installed?")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                bail!("{target}: {}", stderr.trim());
            }
            // A machine whose login hangs can drop the session without a
            // word; an empty reason gives the operator nothing to fix.
            let status = output
                .status
                .code()
                .map_or_else(|| "a signal".to_string(), |code| format!("status {code}"));
            let port = if target.port == 22 {
                String::new()
            } else {
                format!("-p {} ", target.port)
            };
            bail!(
                "{target}: ssh exited with {status} and said nothing; check that the machine \
                 accepts a login (`ssh {port}{} true`)",
                target.destination()
            );
        }
        String::from_utf8(output.stdout).context("ssh returned invalid UTF-8")
    }

    /// Run `script` as the login user, with `args` as its positional parameters.
    pub async fn user_script(
        &self,
        target: &SshTarget,
        script: &str,
        args: &[&str],
    ) -> Result<String> {
        let remote = remote_command(script, args, false);
        self.run(target, remote, std::process::Stdio::null()).await
    }

    /// Run `script` as root through passwordless `sudo`.
    pub async fn root_script(
        &self,
        target: &SshTarget,
        script: &str,
        args: &[&str],
    ) -> Result<String> {
        let remote = remote_command(script, args, true);
        self.run(target, remote, std::process::Stdio::null()).await
    }

    /// Run `script` as root, reading `input` on its standard input.
    pub async fn root_script_with_input(
        &self,
        target: &SshTarget,
        script: &str,
        args: &[&str],
        input: std::fs::File,
    ) -> Result<String> {
        let remote = remote_command(script, args, true);
        self.run(target, remote, std::process::Stdio::from(input))
            .await
    }

    /// Install `files` as root, each replaced in one `rename`, as
    /// [`super::lima::Lima::install_files`] does for a VM.
    pub async fn install_files(&self, target: &SshTarget, files: Vec<GuestFile>) -> Result<()> {
        let script = install_script(&self.root, &files)?;
        let bundle = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let mut bundle = tempfile::tempfile()?;
            write_bundle(&files, &mut bundle)?;
            std::io::Seek::rewind(&mut bundle)?;
            Ok(bundle)
        })
        .await??;
        self.root_script_with_input(target, &script, &[], bundle)
            .await?;
        Ok(())
    }

    /// Forward a loopback port here to `remote_port` on `target` until the
    /// result drops.
    ///
    /// A machine's firewall may not let this one reach its API, but Bun's
    /// perimeter always admits loopback, so setup can work before the
    /// operator's own address is admitted.
    pub async fn tunnel(
        &self,
        target: &SshTarget,
        local_port: u16,
        remote_port: u16,
    ) -> Result<Tunnel> {
        let forward = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
        let mut command = self.command(
            target,
            &["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward],
        );
        command
            .arg("--")
            .arg(target.destination())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().context("failed to execute ssh")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("{target}: the tunnel to port {remote_port} closed ({status})");
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
                .await
                .is_ok()
            {
                return Ok(Tunnel { _child: child });
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("{target}: the tunnel to port {remote_port} didn't open within 15 s");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// A running `ssh -L`, killed when dropped.
#[derive(Debug)]
pub struct Tunnel {
    _child: tokio::process::Child,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(text: &str) -> SshTarget {
        text.parse().unwrap()
    }

    #[test]
    fn a_target_takes_an_optional_user_and_port() {
        assert_eq!(
            target("ubuntu@10.0.0.1:2222"),
            SshTarget {
                user: Some("ubuntu".into()),
                host: "10.0.0.1".into(),
                port: 2222
            }
        );
        assert_eq!(
            target("node-1.lan"),
            SshTarget {
                user: None,
                host: "node-1.lan".into(),
                port: 22
            }
        );
    }

    #[test]
    fn a_target_prints_as_it_was_written() {
        assert_eq!(target("ubuntu@a:2222").to_string(), "ubuntu@a:2222");
        assert_eq!(target("ubuntu@a").to_string(), "ubuntu@a");
    }

    /// `ssh -oProxyCommand=...` runs a command, so a host that starts with a
    /// dash must never reach it, and neither must a shell word.
    #[test]
    fn a_target_that_ssh_could_read_as_an_option_is_refused() {
        for bad in [
            "-oProxyCommand=touch /tmp/x",
            "ubuntu@-oProxyCommand=x",
            "-user@host",
            "a b",
            "a;b",
            "$(x)",
            "",
            "@host",
            "user@",
            "host:",
            "host:0",
            "host:70000",
            "[::1]:22",
        ] {
            assert!(bad.parse::<SshTarget>().is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_list_keeps_its_order_and_refuses_a_repeat() {
        let targets = parse_targets("u@a,u@b:2222,c").unwrap();
        let hosts: Vec<_> = targets.iter().map(|t| t.host.as_str()).collect();
        assert_eq!(hosts, ["a", "b", "c"]);
        assert!(
            parse_targets("u@a,v@a")
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
        // The same address on another port is another machine: a forwarded VM.
        assert_eq!(parse_targets("a:2222,a:2223").unwrap().len(), 2);
        assert!(parse_targets("a,,b").is_err());
    }

    /// Stands in for `ssh`: skips the options, then hands the remote command
    /// to a real shell, so quoting is tested by being unquoted again.
    fn fake_ssh(directory: &std::path::Path, root: &str) -> Ssh {
        let script = directory.join("ssh");
        std::fs::write(
            &script,
            "while [ \"$1\" != -- ]; do shift; done\nshift\nshift\n\
             sudo() { shift; \"$@\"; }\neval \"$1\"\n",
        )
        .unwrap();
        Ssh::shell_script(script, Duration::from_secs(10), root)
    }

    #[tokio::test]
    async fn arguments_reach_the_remote_script_intact() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path(), "");
        let hostile = "it's $HOME `id` \"quoted\" a  b";
        for output in [
            ssh.user_script(&target("a"), "printf '%s' \"$1\"", &[hostile])
                .await,
            ssh.root_script(&target("a"), "printf '%s' \"$1\"", &[hostile])
                .await,
        ] {
            assert_eq!(output.unwrap(), hostile);
        }
    }

    #[tokio::test]
    async fn a_root_script_reads_its_input() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path(), "");
        let input = directory.path().join("input");
        std::fs::write(&input, "token").unwrap();
        let output = ssh
            .root_script_with_input(
                &target("a"),
                "cat",
                &[],
                std::fs::File::open(&input).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(output, "token");
    }

    #[tokio::test]
    async fn a_failure_names_the_machine_and_keeps_the_reason() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path(), "");
        let error = ssh
            .user_script(&target("u@a"), "echo refused >&2; exit 3", &[])
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "u@a: refused");
    }

    /// A machine whose login service hung dropped the session without a word
    /// (an 8-machine run on the Mac Studio, 5 November 2026), and the operator
    /// was told to fix `u@a: ` and nothing else.
    #[tokio::test]
    async fn a_silent_failure_says_how_ssh_exited() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path(), "");
        let error = ssh
            .user_script(&target("u@a"), "exit 255", &[])
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "u@a: ssh exited with status 255 and said nothing; check that the machine \
             accepts a login (`ssh u@a true`)"
        );
    }

    #[tokio::test]
    async fn ssh_never_prompts_and_ends_its_options_before_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("args");
        let script = directory.path().join("ssh");
        std::fs::write(
            &script,
            format!("printf '%s\\n' \"$@\" > {}\n", record.display()),
        )
        .unwrap();
        let ssh = Ssh::shell_script(script, Duration::from_secs(10), "");
        ssh.user_script(&target("u@a:2222"), "true", &[])
            .await
            .unwrap();
        let args = std::fs::read_to_string(record).unwrap();
        let args: Vec<_> = args.lines().collect();
        assert!(args.contains(&"BatchMode=yes"));
        let port = args.iter().position(|arg| *arg == "-p").unwrap();
        assert_eq!(args[port + 1], "2222");
        let end = args.iter().position(|arg| *arg == "--").unwrap();
        assert_eq!(args[end + 1], "u@a");
    }

    #[tokio::test]
    async fn files_install_as_they_do_in_a_vm() {
        let directory = tempfile::tempdir().unwrap();
        let machine = directory.path().join("machine");
        std::fs::create_dir_all(machine.join("var/tmp")).unwrap();
        let ssh = fake_ssh(directory.path(), machine.to_str().unwrap());
        let source = directory.path().join("node.toml");
        std::fs::write(&source, "config").unwrap();
        ssh.install_files(
            &target("a"),
            vec![GuestFile::private(source, "/etc/reliaburger/node.toml")],
        )
        .await
        .unwrap();
        let installed = machine.join("etc/reliaburger/node.toml");
        assert_eq!(std::fs::read_to_string(installed).unwrap(), "config");
    }
}

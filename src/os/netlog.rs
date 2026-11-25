//! Where an appliance sends its boot logs (#2149): `reliaburger/netlog.conf`
//! on its ESP, which `reliaburger-netlog.service` reads at every boot
//! (`image/mkosi.extra/usr/lib/reliaburger/netlog`).
//!
//! `relish netboot --stream-logs` has the installer write the file. After
//! that, `relish os boot-logs --to <address>[:<port>]` and `--off` change
//! it on a claimed cluster: relish asks each node's bun
//! (`/v1/os/boot-logs`), and bun rewrites or deletes the file here. The
//! file is the only state; the next boot reads it. So the change also
//! takes effect now, bun restarts the netlog unit after writing a target,
//! and switches the kernel's netconsole target off after deleting it.
//!
//! The format is the script's: `key=value` lines, `#` comments, and one
//! key, `target=<IPv4 address>[:<port>]`, with 6666 as the port when it
//! isn't given. This module reads the file the same way, so what
//! `relish os boot-logs` shows is what the next boot will do.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The port boot logs go to when a target doesn't name one: netconsole's
/// usual one, and the one `relish netboot` and `relish machines logs`
/// listen on.
pub const DEFAULT_PORT: u16 = 6666;

/// The file's path under the ESP.
pub const CONFIG_FILE: &str = "reliaburger/netlog.conf";

/// Where boot logs go: an IPv4 address and a UDP port. netconsole only
/// takes IPv4 here, as the script does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlogTarget {
    pub address: Ipv4Addr,
    pub port: u16,
}

/// Why a target or the file didn't parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetlogError {
    #[error("{value:?} isn't <IPv4 address>[:<port>]")]
    BadTarget { value: String },
    #[error("{line:?} isn't key=value")]
    NotKeyValue { line: String },
    #[error("it has no target=")]
    NoTarget,
    #[error("{address} isn't one machine on the LAN: use the listener's own address")]
    NotOneMachine { address: Ipv4Addr },
}

impl FromStr for NetlogTarget {
    type Err = NetlogError;

    fn from_str(value: &str) -> Result<Self, NetlogError> {
        let bad = || NetlogError::BadTarget {
            value: value.to_string(),
        };
        let (address, port) = match value.split_once(':') {
            Some((address, port)) => (address, decimal(port).ok_or_else(bad)?),
            None => (value, DEFAULT_PORT),
        };
        if port == 0 {
            return Err(bad());
        }
        // Rust's own parser already takes only four plain decimal numbers,
        // with no sign and no leading zeros, which is the script's rule.
        let address = address.parse::<Ipv4Addr>().map_err(|_| bad())?;
        // One machine's listener, reachable over the LAN: not every machine
        // on it, not a group, not nowhere and not the node itself.
        if address.is_broadcast()
            || address.is_multicast()
            || address.is_unspecified()
            || address.is_loopback()
        {
            return Err(NetlogError::NotOneMachine { address });
        }
        Ok(Self { address, port })
    }
}

/// A port as the boot script reads it: digits only, so no `+`, and no
/// leading zero, which other tools may read as octal.
fn decimal(text: &str) -> Option<u16> {
    let digits_only = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits_only || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for NetlogTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.address, self.port)
    }
}

// As its `address:port` string on the wire, so `-o json` shows what the
// file says.
impl Serialize for NetlogTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NetlogTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The target `netlog.conf` names, as the boot script reads it: blank lines
/// and `#` comments skipped, unknown keys ignored, the last `target=` wins.
pub fn parse_config(text: &str) -> Result<NetlogTarget, NetlogError> {
    let mut target = None;
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(NetlogError::NotKeyValue {
                line: line.to_string(),
            });
        };
        if key == "target" {
            target = Some(value);
        }
    }
    target.ok_or(NetlogError::NoTarget)?.parse()
}

/// The file bun writes for `target`.
pub fn render_config(target: NetlogTarget) -> String {
    format!(
        "# Boot logs go here over UDP, in cleartext (#2149).\n\
         # `relish os boot-logs --off` deletes this file to stop sending them.\n\
         target={target}\n"
    )
}

/// What a node says about its boot logs (`GET /v1/os/boot-logs`), and
/// what a change asks for (`POST`): the target, or `None` for off. Read
/// strictly: `target` must be there, as `null` for off, so a misspelt
/// `{"too": …}` or an empty `{}` is an error rather than a quiet off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootLogs {
    // `deserialize_with` turns off serde's "a missing Option is None".
    #[serde(deserialize_with = "present")]
    pub target: Option<NetlogTarget>,
}

/// An `Option` field that must be in the input, even as `null`.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NetlogTarget>, D::Error> {
    Option::deserialize(deserializer)
}

/// How long the netlog unit may take to restart: above its
/// `TimeoutStartSec=30`, so systemd gives up on the script before this does.
const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// How long reading the unit's journal may take.
const JOURNAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the script starts each line it logs with.
const SCRIPT_PREFIX: &str = "reliaburger-netlog: ";

/// Paths and commands, overridable for tests.
#[derive(Debug, Clone)]
pub struct NetlogPaths {
    /// Where the ESP may be mounted, in the script's order: gpt-auto mounts
    /// it at `/boot`, or at `/efi` when `/boot` isn't empty.
    pub esp_candidates: Vec<PathBuf>,
    /// The netconsole target the script creates in configfs: its
    /// `enabled`, `remote_ip` and `remote_port` say where the kernel's lines
    /// go now.
    pub netconsole: PathBuf,
    /// Restarts the netlog unit and waits for its script to finish, as argv.
    pub restart: Vec<String>,
    /// Prints the netlog unit's journal, one message a line, as argv;
    /// `--since=@<seconds>` goes on the end.
    pub journal: Vec<String>,
}

impl Default for NetlogPaths {
    fn default() -> Self {
        let argv = |words: &[&str]| words.iter().map(|word| word.to_string()).collect();
        Self {
            esp_candidates: vec![PathBuf::from("/boot"), PathBuf::from("/efi")],
            netconsole: PathBuf::from("/sys/kernel/config/netconsole/reliaburger"),
            // Not --no-block: the unit is a oneshot, so this returns when the
            // script has finished, and configfs then says whether it worked.
            restart: argv(&["systemctl", "restart", "reliaburger-netlog.service"]),
            journal: argv(&[
                "journalctl",
                "--unit=reliaburger-netlog.service",
                "--output=cat",
                "--no-pager",
            ]),
        }
    }
}

impl NetlogPaths {
    /// The `netlog.conf` the next boot reads: the first candidate's that
    /// exists, as the script picks it; else, the file on the first
    /// candidate that is the ESP (it has `EFI/` or systemd-boot's
    /// `loader/`). `None` when no candidate is.
    pub fn config_file(&self) -> Option<PathBuf> {
        let file = |esp: &PathBuf| esp.join(CONFIG_FILE);
        self.esp_candidates
            .iter()
            .map(file)
            .find(|path| path.exists())
            .or_else(|| {
                self.esp_candidates
                    .iter()
                    .find(|esp| esp.join("EFI").is_dir() || esp.join("loader").is_dir())
                    .map(file)
            })
    }
}

/// What the next boot will do: the target in `netlog.conf`, or `None` when
/// there's no file. A file that doesn't parse is an error, since the boot
/// would ignore it.
pub fn read(paths: &NetlogPaths) -> Result<Option<NetlogTarget>, String> {
    let Some(path) = paths.config_file() else {
        return Err("no ESP at /boot or /efi".to_string());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_config(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}; the next boot ignores it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write `target` to `netlog.conf`, or delete the file for `None`.
/// Deleting a file that isn't there succeeds.
pub fn write(paths: &NetlogPaths, target: Option<NetlogTarget>) -> Result<(), String> {
    let Some(path) = paths.config_file() else {
        return Err("no ESP at /boot or /efi".to_string());
    };
    let result = match target {
        Some(target) => {
            crate::appliance::prepare::write_atomic(&path, render_config(target).as_bytes(), 0o644)
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// Make the change take effect now as well as at the next boot: restart the
/// netlog unit for a target (it reconfigures netconsole and says hello),
/// then check that netconsole now sends to it; or switch netconsole's target
/// off. Best effort: what it returns is a warning, since the file is already
/// written.
pub async fn apply_now(paths: &NetlogPaths, target: Option<NetlogTarget>) -> Option<String> {
    let Some(target) = target else {
        return switch_off_now(paths).await;
    };
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if let Err(reason) = run(&paths.restart, RESTART_TIMEOUT).await {
        return Some(format!("{reason}; boot logs start at the next boot"));
    }
    // The unit's script exits 0 whatever happens, so that a boot never
    // fails over its logs: configfs says whether it worked (#2323).
    let found = netconsole_mismatch(&paths.netconsole, target).await?;
    let mut journal = paths.journal.clone();
    journal.push(format!("--since=@{since}"));
    let reason = run(&journal, JOURNAL_TIMEOUT)
        .await
        .ok()
        .and_then(|lines| script_reason(&lines))
        .unwrap_or(found);
    Some(format!(
        "not streaming now: {reason}; the next boot tries again"
    ))
}

/// Switch the netconsole target off, if the script made one.
async fn switch_off_now(paths: &NetlogPaths) -> Option<String> {
    let enabled = paths.netconsole.join("enabled");
    if !enabled.exists() {
        return None;
    }
    tokio::fs::write(&enabled, b"0\n").await.err().map(|e| {
        format!(
            "{}: {e}; boot logs stop at the next boot",
            enabled.display()
        )
    })
}

/// Run `argv` to the end within `limit`, and answer what it printed, or why
/// it failed.
async fn run(argv: &[String], limit: std::time::Duration) -> Result<String, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("no command".to_string());
    };
    // Killed if the timeout below drops it, not left behind.
    let output = tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(limit, output).await {
        Ok(Ok(output)) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(Ok(output)) => Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Err(e)) => Err(format!("{program}: {e}")),
        Err(_) => Err(format!("{program} took over {} s", limit.as_secs())),
    }
}

/// `None` when the netconsole target in `dir` is on and sends to `target`;
/// else what it found instead.
async fn netconsole_mismatch(dir: &Path, target: NetlogTarget) -> Option<String> {
    let read = |name: &str| {
        let path = dir.join(name);
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|text| text.trim().to_string())
        }
    };
    let Ok(enabled) = read("enabled").await else {
        return Some("the kernel has no netconsole target".to_string());
    };
    if enabled != "1" {
        return Some("the kernel's netconsole target is off".to_string());
    }
    let address = read("remote_ip").await.unwrap_or_default();
    let port = read("remote_port").await.unwrap_or_default();
    if address == target.address.to_string() && port == target.port.to_string() {
        return None;
    }
    Some(format!("the kernel's netconsole sends to {address}:{port}"))
}

/// The script's last word in its journal lines: why it isn't streaming.
fn script_reason(lines: &str) -> Option<String> {
    lines
        .lines()
        .filter_map(|line| line.trim().strip_prefix(SCRIPT_PREFIX))
        .next_back()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(value: &str) -> NetlogTarget {
        value.parse().unwrap()
    }

    #[test]
    fn a_target_takes_an_ipv4_address_and_an_optional_port() {
        assert_eq!(
            target("10.42.0.1"),
            NetlogTarget {
                address: Ipv4Addr::new(10, 42, 0, 1),
                port: DEFAULT_PORT
            }
        );
        assert_eq!(target("192.168.1.20:7000").port, 7000);
        assert_eq!(target("10.0.0.1:1").port, 1);
        assert_eq!(target("10.0.0.1:65535").port, 65535);
        assert_eq!(
            target("10.0.0.10:6666").address,
            Ipv4Addr::new(10, 0, 0, 10)
        );
        assert_eq!(target("10.0.0.0:6666").address, Ipv4Addr::new(10, 0, 0, 0));
        assert_eq!(target("10.42.0.1").to_string(), "10.42.0.1:6666");
    }

    #[test]
    fn a_target_says_when_it_names_no_one_machine() {
        assert_eq!(
            "255.255.255.255".parse::<NetlogTarget>(),
            Err(NetlogError::NotOneMachine {
                address: Ipv4Addr::BROADCAST
            })
        );
    }

    #[test]
    fn a_target_refuses_anything_but_one_ipv4_listener() {
        for value in [
            "",
            "laptop.local",
            "10.42.0",
            "10.42.0.256",
            "10.42.0.1:",
            "10.42.0.1:0",
            "10.42.0.1:65536",
            "10.42.0.1:port",
            "::1",
            "[fe80::1]:6666",
            "255.255.255.255",
            "224.0.0.1",
            "239.1.2.3:6666",
            "0.0.0.0",
            "127.0.0.1",
            "127.1.2.3:6666",
            // What the boot script refuses too (#2323): a sign, a leading
            // zero, a space, more than one colon.
            "10.42.0.1:+7000",
            "10.42.0.1:-1",
            "10.42.0.1:07000",
            "10.42.0.1:00",
            "10.42.0.1: 7000",
            "10.42.0.1:7000:1",
            "010.42.0.1",
            "10.042.0.1:6666",
            "10.42.0.01",
            "+10.42.0.1",
        ] {
            assert!(
                value.parse::<NetlogTarget>().is_err(),
                "{value:?} should be refused"
            );
        }
    }

    #[test]
    fn the_config_is_read_as_the_boot_script_reads_it() {
        let text = "# Boot logs go here\r\n\nfuture=1\ntarget=10.0.0.1\ntarget=10.0.0.2:7000\n";
        assert_eq!(parse_config(text), Ok(target("10.0.0.2:7000")));
        assert_eq!(parse_config("# nothing\n"), Err(NetlogError::NoTarget));
        assert_eq!(
            parse_config("target 10.0.0.1\n"),
            Err(NetlogError::NotKeyValue {
                line: "target 10.0.0.1".into()
            })
        );
        assert!(matches!(
            parse_config("target=laptop\n"),
            Err(NetlogError::BadTarget { .. })
        ));
    }

    #[test]
    fn what_bun_writes_reads_back_as_the_same_target() {
        let written = target("10.42.0.1:6666");
        assert_eq!(parse_config(&render_config(written)), Ok(written));
    }

    #[test]
    fn boot_logs_are_a_target_string_or_null_as_data() {
        let on = BootLogs {
            target: Some(target("10.42.0.1")),
        };
        assert_eq!(
            serde_json::to_value(&on).unwrap(),
            serde_json::json!({"target": "10.42.0.1:6666"})
        );
        let off: BootLogs = serde_json::from_str(r#"{"target": null}"#).unwrap();
        assert_eq!(off.target, None);
        assert!(serde_json::from_str::<BootLogs>(r#"{"target": "laptop"}"#).is_err());
    }

    #[test]
    fn off_has_to_be_asked_for_by_name() {
        for body in [
            "{}",
            r#"{"too": "10.42.0.1"}"#,
            r#"{"target": null, "off": true}"#,
        ] {
            assert!(
                serde_json::from_str::<BootLogs>(body).is_err(),
                "{body} should be refused"
            );
        }
    }

    fn esps(dir: &Path) -> NetlogPaths {
        NetlogPaths {
            esp_candidates: vec![dir.join("boot"), dir.join("efi")],
            netconsole: dir.join("netconsole"),
            restart: vec!["true".into()],
            journal: vec!["true".into()],
        }
    }

    #[test]
    fn the_file_goes_on_whichever_mount_point_is_the_esp() {
        let dir = tempfile::tempdir().unwrap();
        let paths = esps(dir.path());
        std::fs::create_dir_all(dir.path().join("boot")).unwrap();
        assert_eq!(paths.config_file(), None, "an empty /boot isn't the ESP");
        assert!(read(&paths).is_err());
        std::fs::create_dir_all(dir.path().join("efi/EFI")).unwrap();
        assert_eq!(
            paths.config_file(),
            Some(dir.path().join("efi/reliaburger/netlog.conf"))
        );
    }

    #[test]
    fn an_existing_file_wins_as_it_does_for_the_boot_script() {
        let dir = tempfile::tempdir().unwrap();
        let paths = esps(dir.path());
        std::fs::create_dir_all(dir.path().join("boot/loader")).unwrap();
        std::fs::create_dir_all(dir.path().join("efi/reliaburger")).unwrap();
        std::fs::write(
            dir.path().join("efi/reliaburger/netlog.conf"),
            "target=10.0.0.9\n",
        )
        .unwrap();
        assert_eq!(read(&paths), Ok(Some(target("10.0.0.9"))));
    }

    #[test]
    fn writing_then_switching_off_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = esps(dir.path());
        std::fs::create_dir_all(dir.path().join("boot/EFI")).unwrap();
        assert_eq!(read(&paths), Ok(None));
        write(&paths, Some(target("10.42.0.1:7000"))).unwrap();
        assert_eq!(read(&paths), Ok(Some(target("10.42.0.1:7000"))));
        write(&paths, None).unwrap();
        assert_eq!(read(&paths), Ok(None));
        write(&paths, None).unwrap();
    }

    #[test]
    fn a_file_the_boot_would_ignore_reads_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = esps(dir.path());
        std::fs::create_dir_all(dir.path().join("boot/reliaburger")).unwrap();
        std::fs::write(
            dir.path().join("boot/reliaburger/netlog.conf"),
            "target=x\n",
        )
        .unwrap();
        let error = read(&paths).unwrap_err();
        assert!(error.contains("the next boot ignores it"), "{error}");
    }

    #[tokio::test]
    async fn switching_off_now_disables_the_netconsole_target() {
        let dir = tempfile::tempdir().unwrap();
        let paths = esps(dir.path());
        assert_eq!(
            apply_now(&paths, None).await,
            None,
            "no target, nothing to do"
        );
        let enabled = paths.netconsole.join("enabled");
        std::fs::create_dir_all(&paths.netconsole).unwrap();
        std::fs::write(&enabled, "1\n").unwrap();
        assert_eq!(apply_now(&paths, None).await, None);
        assert_eq!(std::fs::read_to_string(&enabled).unwrap(), "0\n");
    }

    /// A netconsole target in configfs as the kernel shows it.
    fn netconsole(paths: &NetlogPaths, enabled: &str, address: &str, port: &str) {
        std::fs::create_dir_all(&paths.netconsole).unwrap();
        for (name, value) in [
            ("enabled", enabled),
            ("remote_ip", address),
            ("remote_port", port),
        ] {
            std::fs::write(paths.netconsole.join(name), format!("{value}\n")).unwrap();
        }
    }

    /// A journal that prints `lines`, whatever it's asked.
    fn journal(lines: &str) -> Vec<String> {
        vec![
            "sh".into(),
            "-c".into(),
            format!("printf '{lines}'"),
            "journalctl".into(),
        ]
    }

    #[tokio::test]
    async fn a_restart_that_fails_is_a_warning_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = esps(dir.path());
        paths.restart = vec!["false".into()];
        let warning = apply_now(&paths, Some(target("10.0.0.1"))).await.unwrap();
        assert!(warning.contains("next boot"), "{warning}");
    }

    #[tokio::test]
    async fn a_netconsole_sending_to_the_target_needs_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = esps(dir.path());
        netconsole(&paths, "1", "10.0.0.1", "7000");
        paths.journal = journal("reliaburger-netlog: no route to 10.0.0.1; not streaming\\n");
        assert_eq!(apply_now(&paths, Some(target("10.0.0.1:7000"))).await, None);
    }

    #[tokio::test]
    async fn a_script_that_didnt_stream_is_a_warning_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = esps(dir.path());
        paths.journal = journal(
            "Starting reliaburger-netlog.service\\n\
             reliaburger-netlog: no route to 10.0.0.1; not streaming\\n",
        );
        // The unit restarted and exited 0, but netconsole has no target.
        assert_eq!(
            apply_now(&paths, Some(target("10.0.0.1"))).await,
            Some(
                "not streaming now: no route to 10.0.0.1; not streaming; the next boot tries again"
                    .into()
            )
        );
        // An old target, still on: the script gave up before changing it.
        netconsole(&paths, "1", "10.0.0.9", "6666");
        paths.journal = journal("");
        assert_eq!(
            apply_now(&paths, Some(target("10.0.0.1"))).await,
            Some(
                "not streaming now: the kernel's netconsole sends to 10.0.0.9:6666; \
                 the next boot tries again"
                    .into()
            )
        );
        netconsole(&paths, "0", "10.0.0.1", "6666");
        let warning = apply_now(&paths, Some(target("10.0.0.1"))).await.unwrap();
        assert!(warning.contains("target is off"), "{warning}");
        netconsole(&paths, "1", "10.0.0.1", "6667");
        assert!(apply_now(&paths, Some(target("10.0.0.1"))).await.is_some());
    }
}

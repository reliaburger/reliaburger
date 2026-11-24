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
use std::path::PathBuf;
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
}

impl FromStr for NetlogTarget {
    type Err = NetlogError;

    fn from_str(value: &str) -> Result<Self, NetlogError> {
        let bad = || NetlogError::BadTarget {
            value: value.to_string(),
        };
        let (address, port) = match value.split_once(':') {
            Some((address, port)) => (address, port.parse::<u16>().map_err(|_| bad())?),
            None => (value, DEFAULT_PORT),
        };
        if port == 0 {
            return Err(bad());
        }
        let address = address.parse::<Ipv4Addr>().map_err(|_| bad())?;
        Ok(Self { address, port })
    }
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
/// what a change asks for (`POST`): the target, or `None` for off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootLogs {
    pub target: Option<NetlogTarget>,
}

/// Paths and commands, overridable for tests.
#[derive(Debug, Clone)]
pub struct NetlogPaths {
    /// Where the ESP may be mounted, in the script's order: gpt-auto mounts
    /// it at `/boot`, or at `/efi` when `/boot` isn't empty.
    pub esp_candidates: Vec<PathBuf>,
    /// The netconsole target the script creates in configfs.
    pub netconsole_enabled: PathBuf,
    /// Restarts the netlog unit without waiting for it, as argv.
    pub restart: Vec<String>,
}

impl Default for NetlogPaths {
    fn default() -> Self {
        Self {
            esp_candidates: vec![PathBuf::from("/boot"), PathBuf::from("/efi")],
            netconsole_enabled: PathBuf::from("/sys/kernel/config/netconsole/reliaburger/enabled"),
            restart: vec![
                "systemctl".into(),
                "restart".into(),
                "--no-block".into(),
                "reliaburger-netlog.service".into(),
            ],
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
/// netlog unit for a target (it reconfigures netconsole and says hello), or
/// switch netconsole's target off. Best effort: what it returns is a
/// warning, since the file is already written.
pub async fn apply_now(paths: &NetlogPaths, target: Option<NetlogTarget>) -> Option<String> {
    match target {
        Some(_) => {
            let (program, args) = paths.restart.split_first()?;
            let run = tokio::process::Command::new(program).args(args).output();
            match tokio::time::timeout(std::time::Duration::from_secs(10), run).await {
                Ok(Ok(output)) if output.status.success() => None,
                Ok(Ok(output)) => Some(format!(
                    "{program} failed: {}; boot logs start at the next boot",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Ok(Err(e)) => Some(format!("{program}: {e}; boot logs start at the next boot")),
                Err(_) => Some(format!(
                    "{program} took over 10 s; boot logs start at the next boot"
                )),
            }
        }
        None => {
            if !paths.netconsole_enabled.exists() {
                return None;
            }
            tokio::fs::write(&paths.netconsole_enabled, b"0\n")
                .await
                .err()
                .map(|e| {
                    format!(
                        "{}: {e}; boot logs stop at the next boot",
                        paths.netconsole_enabled.display()
                    )
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        assert_eq!(target("10.42.0.1").to_string(), "10.42.0.1:6666");
    }

    #[test]
    fn a_target_refuses_what_the_boot_script_would_ignore() {
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

    fn esps(dir: &Path) -> NetlogPaths {
        NetlogPaths {
            esp_candidates: vec![dir.join("boot"), dir.join("efi")],
            netconsole_enabled: dir.join("enabled"),
            restart: vec!["true".into()],
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
        std::fs::write(&paths.netconsole_enabled, "1\n").unwrap();
        assert_eq!(apply_now(&paths, None).await, None);
        assert_eq!(
            std::fs::read_to_string(&paths.netconsole_enabled).unwrap(),
            "0\n"
        );
    }

    #[tokio::test]
    async fn a_restart_that_fails_is_a_warning_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = esps(dir.path());
        assert_eq!(apply_now(&paths, Some(target("10.0.0.1"))).await, None);
        paths.restart = vec!["false".into()];
        let warning = apply_now(&paths, Some(target("10.0.0.1"))).await.unwrap();
        assert!(warning.contains("next boot"), "{warning}");
    }
}

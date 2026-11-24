//! What the console reads from the machine itself, with bun or without it:
//! who the node is, its versions, its load and its journal.
//!
//! Everything here works while bun is down, which is when the screen
//! matters most: during a boot, a fallback, or an unclaimed machine's wait.
//! Every file read and command runs off the async runtime (tokio's file
//! and process APIs, or `spawn_blocking`) and every command has a deadline,
//! so a slow disk or a hung `journalctl` can't freeze the screen.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where bun's binary lives on an appliance.
const BUN_BINARY: &str = "/var/lib/reliaburger/bin/bun";

/// The data partition, whose fill the Metrics tab shows.
pub const DATA_PATH: &str = "/var/lib/reliaburger";

/// How long a `--version` or `journalctl` gets before it counts as failed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The units whose journal the Logs tab shows.
pub const LOG_UNITS: [&str; 2] = ["reliaburger.service", "reliaburger-boot-check.service"];

/// Who this machine is and whether bun answers: what the console showed
/// before it had tabs, and what it still shows when bun doesn't answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalStatus {
    pub os_version: Option<String>,
    pub bun_version: Option<String>,
    pub node: Option<String>,
    pub cluster: Option<String>,
    pub address: Option<String>,
    pub macs: Vec<String>,
    /// From bun's `/v1/health`: `None` when bun doesn't answer.
    pub healthy: Option<bool>,
    /// An unclaimed machine's claim key, as a person compares it
    /// (`crate::appliance::claim::short_fingerprint`).
    pub claim_key: Option<String>,
    /// Lines from `bun appliance prepare`, when it's still working (an
    /// unseeded node, or one enrolling).
    pub notes: Vec<String>,
}

impl LocalStatus {
    /// Whether bun answered its health check.
    pub fn bun_up(&self) -> bool {
        self.healthy == Some(true)
    }

    /// Whether the machine still waits for its claim, so its key is what
    /// the screen is for.
    pub fn waiting_for_claim(&self) -> bool {
        self.node.is_none() && self.claim_key.is_some()
    }
}

/// The versions of what the appliance runs, for the Versions tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Versions {
    /// The OS image's `IMAGE_VERSION`.
    pub os_image: Option<String>,
    /// The running kernel's release.
    pub kernel: Option<String>,
    /// The bun binary on disk.
    pub bun: Option<String>,
    pub runc: Option<String>,
    pub systemd: Option<String>,
    /// `None` when Buildah isn't installed.
    pub buildah: Option<String>,
}

/// Reads [`LocalStatus`], keeping bun's version between reads.
pub struct Gatherer {
    paths: crate::appliance::Paths,
    bun_binary: PathBuf,
    /// bun's version, and the binary's modification time when it was read.
    bun_version: tokio::sync::Mutex<Option<(std::time::SystemTime, Option<String>)>>,
}

impl Gatherer {
    pub fn new(paths: crate::appliance::Paths) -> Self {
        Self::with_bun_binary(paths, PathBuf::from(BUN_BINARY))
    }

    fn with_bun_binary(paths: crate::appliance::Paths, bun_binary: PathBuf) -> Self {
        Self {
            paths,
            bun_binary,
            bun_version: tokio::sync::Mutex::new(None),
        }
    }

    /// bun's version, running `bun --version` only when the binary has
    /// changed since the last time: an upgrade replaces it, and nothing else
    /// does.
    async fn bun_version(&self) -> Option<String> {
        let modified = tokio::fs::metadata(&self.bun_binary)
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        let mut cached = self.bun_version.lock().await;
        if let (Some(modified), Some((seen, version))) = (modified, cached.as_ref())
            && modified == *seen
        {
            return version.clone();
        }
        let version = command_version(&self.bun_binary, parse_bun_version).await;
        *cached = modified.map(|modified| (modified, version.clone()));
        version
    }

    /// Read who this machine is, and ask bun whether it's healthy.
    pub async fn gather(&self) -> LocalStatus {
        let paths = &self.paths;
        let os_version = os_image_version().await;
        let node_config = tokio::fs::read_to_string(paths.config_dir.join("node.toml"))
            .await
            .ok()
            .and_then(|text| toml::from_str::<crate::config::node::NodeConfig>(&text).ok());
        let claim_key = match &paths.claim_dir {
            Some(dir) => tokio::fs::read(dir.join("claim.crt"))
                .await
                .ok()
                .map(|der| {
                    crate::appliance::claim::short_fingerprint(
                        &crate::appliance::claim::certificate_fingerprint(&der),
                    )
                }),
            None => None,
        };
        // The address and the MACs come from the network stack and sysfs.
        let net_dir = paths.net_dir.clone();
        let (address, macs) = tokio::task::spawn_blocking(move || {
            let address = crate::appliance::address::detect().map(|a| a.to_string());
            let macs = crate::appliance::prepare::stick_names(&net_dir)
                .into_iter()
                .map(|name| name.replace('-', ":"))
                .collect::<Vec<_>>();
            (address, macs)
        })
        .await
        .unwrap_or_default();
        LocalStatus {
            os_version,
            bun_version: self.bun_version().await,
            node: node_config.as_ref().and_then(|c| c.node.name.clone()),
            cluster: node_config.as_ref().map(|c| c.cluster.name.clone()),
            address,
            macs,
            healthy: healthy().await,
            claim_key,
            notes: Vec::new(),
        }
    }
}

/// The versions the Versions tab shows. Each one is looked up on its own,
/// so a missing tool leaves only its own line empty.
pub async fn versions() -> Versions {
    let kernel = tokio::fs::read_to_string("/proc/sys/kernel/osrelease")
        .await
        .ok()
        .map(|release| release.trim().to_string())
        .filter(|release| !release.is_empty());
    let (bun, runc, systemd, buildah) = tokio::join!(
        command_version(BUN_BINARY, parse_bun_version),
        command_version("runc", parse_runc_version),
        command_version("systemctl", parse_systemd_version),
        command_version("buildah", parse_buildah_version),
    );
    Versions {
        os_image: os_image_version().await,
        kernel,
        bun,
        runc,
        systemd,
        buildah,
    }
}

/// The newest `lines` journal lines of [`LOG_UNITS`], oldest first, each
/// as `HH:MM:SS source: message`.
pub async fn journal(lines: usize) -> Result<Vec<String>, String> {
    let mut command = tokio::process::Command::new("journalctl");
    command
        .args(["--no-pager", "--output=json", "--lines"])
        .arg(lines.to_string());
    for unit in LOG_UNITS {
        command.arg("--unit").arg(unit);
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| "journalctl took longer than 5 s".to_string())?
        .map_err(|error| format!("can't run journalctl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("journalctl failed: {}", stderr.trim()));
    }
    Ok(parse_journal(&String::from_utf8_lossy(&output.stdout)))
}

/// `journalctl --output=json` lines as the Logs tab prints them. A line
/// that isn't a journal record is skipped; a message that isn't text (the
/// journal stores those as byte arrays) says so.
pub fn parse_journal(json_lines: &str) -> Vec<String> {
    json_lines
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|record| {
            let time = record["__REALTIME_TIMESTAMP"]
                .as_str()
                .and_then(|micros| micros.parse::<u64>().ok())
                .map(|micros| clock(micros / 1_000_000))
                .unwrap_or_else(|| "--:--:--".to_string());
            let source = record["SYSLOG_IDENTIFIER"]
                .as_str()
                .or_else(|| record["_SYSTEMD_UNIT"].as_str())
                .unwrap_or("?");
            let message = match &record["MESSAGE"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Null => String::new(),
                _ => "(binary message)".to_string(),
            };
            format!("{time} {source}: {message}")
        })
        .collect()
}

/// UTC `HH:MM:SS` of a Unix time. The appliance runs on UTC.
pub fn clock(unix_seconds: u64) -> String {
    let seconds = unix_seconds % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

async fn os_image_version() -> Option<String> {
    let os_release = tokio::fs::read_to_string("/usr/lib/os-release")
        .await
        .unwrap_or_default();
    os_release
        .lines()
        .find_map(|line| line.strip_prefix("IMAGE_VERSION="))
        .map(|v| v.trim_matches('"').to_string())
}

/// Run `program --version` and pick the version out of its first line.
async fn command_version(
    program: impl AsRef<std::ffi::OsStr>,
    parse: fn(&str) -> Option<String>,
) -> Option<String> {
    let mut command = tokio::process::Command::new(program.as_ref());
    command.arg("--version").kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    parse(text.lines().next()?)
}

/// `bun 0.3.3 (abc1234)` → `0.3.3`.
fn parse_bun_version(line: &str) -> Option<String> {
    line.split_whitespace().nth(1).map(str::to_string)
}

/// `runc version 1.1.12` → `1.1.12`.
fn parse_runc_version(line: &str) -> Option<String> {
    line.strip_prefix("runc version ")
        .map(|rest| rest.trim().to_string())
}

/// `systemd 255 (255.4-1ubuntu8)` → `255 (255.4-1ubuntu8)`.
fn parse_systemd_version(line: &str) -> Option<String> {
    line.strip_prefix("systemd ")
        .map(|rest| rest.trim().to_string())
}

/// `buildah version 1.33.7 (image-spec 1.1.0, runtime-spec 1.1.0)` →
/// `1.33.7`.
fn parse_buildah_version(line: &str) -> Option<String> {
    line.strip_prefix("buildah version ")?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// bun's local `/v1/health`, over plain HTTP or the node's own TLS. It's
/// public and carries no token, so it doesn't verify the certificate:
/// the node's certificate names its LAN address, not loopback.
async fn healthy() -> Option<bool> {
    let client = crate::http_client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    for url in [
        "http://127.0.0.1:9117/v1/health",
        "https://127.0.0.1:9117/v1/health",
    ] {
        if let Ok(response) = client.get(url).send().await {
            return Some(response.status().is_success());
        }
    }
    None
}

/// One reading of this machine's load, for the Metrics tab.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    /// All cores together, 0 to 100.
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    /// The data partition's used and total bytes; `None` if it can't be
    /// measured.
    pub disk: Option<(u64, u64)>,
    /// Bytes per second over every interface but loopback.
    pub receive_bytes_per_second: u64,
    pub transmit_bytes_per_second: u64,
}

/// How often the sampler reads the machine's load.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// Read this machine's load every [`SAMPLE_EVERY`] on a thread of its own,
/// sending each [`Sample`] through `send` until it returns `false` (the
/// console has gone).
///
/// A plain thread, not a task: `sysinfo` reads `/proc` with blocking
/// calls, and the CPU figure is the change between two readings, so the
/// reader keeps its state from one sample to the next.
pub fn spawn_sampler(data_path: PathBuf, send: impl Fn(Sample) -> bool + Send + 'static) {
    std::thread::spawn(move || {
        let mut system = sysinfo::System::new();
        let mut networks = sysinfo::Networks::new_with_refreshed_list();
        system.refresh_cpu_usage();
        loop {
            std::thread::sleep(SAMPLE_EVERY);
            system.refresh_cpu_usage();
            system.refresh_memory();
            networks.refresh(true);
            let (received, transmitted) = networks
                .iter()
                .filter(|(name, _)| name.as_str() != "lo")
                .fold((0, 0), |(rx, tx), (_, data)| {
                    (rx + data.received(), tx + data.transmitted())
                });
            let seconds = SAMPLE_EVERY.as_secs().max(1);
            let sample = Sample {
                cpu_percent: system.global_cpu_usage(),
                memory_used_bytes: system.used_memory(),
                memory_total_bytes: system.total_memory(),
                disk: disk_usage(&data_path),
                receive_bytes_per_second: received / seconds,
                transmit_bytes_per_second: transmitted / seconds,
            };
            if !send(sample) {
                return;
            }
        }
    });
}

fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let usage = crate::bun::disk_pressure::FilesystemUsage::of(path).ok()?;
    Some((usage.used_bytes(), usage.total_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_picked_out_of_each_tools_first_line() {
        assert_eq!(
            parse_bun_version("bun 0.3.3 (abc1234)").as_deref(),
            Some("0.3.3")
        );
        assert_eq!(
            parse_runc_version("runc version 1.1.12").as_deref(),
            Some("1.1.12")
        );
        assert_eq!(
            parse_systemd_version("systemd 255 (255.4-1ubuntu8)").as_deref(),
            Some("255 (255.4-1ubuntu8)")
        );
        assert_eq!(
            parse_buildah_version("buildah version 1.33.7 (image-spec 1.1.0, runtime-spec 1.1.0)")
                .as_deref(),
            Some("1.33.7")
        );
        assert_eq!(parse_runc_version("something else"), None);
        assert_eq!(parse_buildah_version(""), None);
    }

    #[test]
    fn journal_records_become_time_source_and_message() {
        let json = concat!(
            r#"{"__REALTIME_TIMESTAMP":"1750000000123456","SYSLOG_IDENTIFIER":"bun","MESSAGE":"bun: listening on 9117"}"#,
            "\n",
            r#"{"__REALTIME_TIMESTAMP":"1750000061000000","_SYSTEMD_UNIT":"reliaburger-boot-check.service","MESSAGE":"bun healthy"}"#,
            "\n",
            r#"{"__REALTIME_TIMESTAMP":"1750000062000000","SYSLOG_IDENTIFIER":"bun","MESSAGE":[104,105]}"#,
            "\n",
            "not json\n",
        );
        assert_eq!(
            parse_journal(json),
            [
                "15:06:40 bun: bun: listening on 9117",
                "15:07:41 reliaburger-boot-check.service: bun healthy",
                "15:07:42 bun: (binary message)",
            ]
        );
    }

    /// `bun --version` runs once per binary: again only when an upgrade
    /// has replaced it, not on every five-second refresh.
    #[tokio::test]
    async fn bun_is_asked_its_version_only_when_its_binary_changes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let bun = dir.path().join("bun");
        // Written beside it and renamed in, so no writable descriptor to
        // the file being run is open (Linux refuses to run one: ETXTBSY).
        let staged = dir.path().join("bun.new");
        let write = |version: &str, modified: std::time::SystemTime| {
            std::fs::write(
                &staged,
                format!(
                    "#!/bin/sh\necho run >> '{}'\necho 'bun {version} (abc1234)'\n",
                    runs.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
            let file = std::fs::File::options().write(true).open(&staged).unwrap();
            file.set_modified(modified).unwrap();
            drop(file);
            std::fs::rename(&staged, &bun).unwrap();
        };
        let then =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000);
        write("0.3.3", then);
        let paths = crate::appliance::Paths {
            config_dir: dir.path().join("etc"),
            credentials_dir: None,
            stick_device: dir.path().join("stick"),
            stick_mount: dir.path().join("mnt"),
            stick_wait: std::time::Duration::ZERO,
            net_dir: dir.path().join("net"),
            claim_dir: None,
        };
        let gatherer = Gatherer::with_bun_binary(paths, bun.clone());
        assert_eq!(gatherer.bun_version().await.as_deref(), Some("0.3.3"));
        assert_eq!(gatherer.bun_version().await.as_deref(), Some("0.3.3"));
        let count = || std::fs::read_to_string(&runs).unwrap().lines().count();
        assert_eq!(count(), 1, "asked again with the binary unchanged");

        write("0.3.4", then + std::time::Duration::from_secs(60));
        assert_eq!(gatherer.bun_version().await.as_deref(), Some("0.3.4"));
        assert_eq!(count(), 2);
    }

    #[test]
    fn an_unseeded_machine_with_a_key_waits_for_its_claim() {
        let waiting = LocalStatus {
            claim_key: Some("3f9a-12bc-77de-0a41".into()),
            ..LocalStatus::default()
        };
        assert!(waiting.waiting_for_claim());
        let seeded = LocalStatus {
            node: Some("home-1".into()),
            ..waiting.clone()
        };
        assert!(!seeded.waiting_for_claim());
        assert!(!LocalStatus::default().waiting_for_claim());
    }
}

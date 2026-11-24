//! Actionable host checks before allocating VM resources.

use anyhow::{Context, Result, bail};
use std::{path::Path, time::Duration};

const GIB: u64 = 1024 * 1024 * 1024;

/// Check supported hardware, VM prerequisites and enough disk for cached downloads.
pub async fn host(root: &Path) -> Result<()> {
    if !matches!(std::env::consts::ARCH, "aarch64" | "x86_64")
        || !matches!(std::env::consts::OS, "macos" | "linux")
    {
        bail!("quickstart requires macOS or Linux on arm64 or x86_64");
    }
    if std::thread::available_parallelism()?.get() < 2 {
        bail!("quickstart requires at least two available CPU cores");
    }
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if free_disk(&root)? < 2 * GIB { bail!("quickstart needs at least 2 GiB free for verified downloads"); }
        #[cfg(target_os = "linux")]
        std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm")
            .context("quickstart requires KVM; enable virtualisation and grant this user access to /dev/kvm")?;
        Ok(())
    }).await??;
    #[cfg(target_os = "linux")]
    qemu().await?;
    #[cfg(target_os = "macos")]
    {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new("/usr/bin/sw_vers")
                .arg("-productVersion")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        let version = String::from_utf8(output.stdout)?;
        let major: u32 = version
            .trim()
            .split('.')
            .next()
            .context("missing macOS version")?
            .parse()?;
        if !output.status.success() || major < 13 {
            bail!("quickstart requires macOS 13 or later for Lima's VZ driver");
        }
    }
    Ok(())
}

/// Check that the host's QEMU is installed and won't abort under Lima's
/// `user-v2` network (#1495). Lima drives QEMU on Linux, so this runs before
/// setup and before `relish local start` boots a VM.
#[cfg(target_os = "linux")]
pub async fn qemu() -> Result<()> {
    let command = format!("qemu-system-{}", std::env::consts::ARCH);
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&command)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| format!("{command} --version did not answer within 5 seconds"))?
    .with_context(|| format!("install {command}: Lima runs the VMs with it"))?;
    if !output.status.success() {
        bail!("{command} did not pass its version check");
    }
    let version = QemuVersion::parse(&String::from_utf8_lossy(&output.stdout));
    let sysctl = tokio::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .await
        .ok();
    check_qemu(&command, version, IoUring::from_sysctl(sysctl.as_deref()))
}

/// A QEMU release, as `qemu-system-* --version` prints it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct QemuVersion {
    major: u32,
    minor: u32,
    micro: u32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl QemuVersion {
    /// The first release whose main loop waits on file descriptors with
    /// io_uring when the kernel allows it, rather than glib's `ppoll(2)`.
    const IO_URING_MAIN_LOOP: QemuVersion = QemuVersion {
        major: 10,
        minor: 2,
        micro: 0,
    };

    /// Read `QEMU emulator version 11.1.3 (qemu-11.1.3-1.fc45)`; distributions
    /// append their own package version in brackets, which we ignore.
    fn parse(text: &str) -> Option<Self> {
        let rest = text
            .lines()
            .next()?
            .strip_prefix("QEMU emulator version ")?;
        let number = rest.split_whitespace().next()?;
        let mut parts = number.split('.').map(str::parse::<u32>);
        Some(QemuVersion {
            major: parts.next()?.ok()?,
            minor: parts.next()?.ok()?,
            micro: parts.next().unwrap_or(Ok(0)).ok()?,
        })
    }
}

impl std::fmt::Display for QemuVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// Whether an unprivileged process such as QEMU may create an io_uring,
/// from the kernel's `kernel.io_uring_disabled` sysctl.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoUring {
    /// `0`, or a kernel before 6.6 that has no switch.
    Enabled,
    /// `1`: only root and members of `kernel.io_uring_group` may use it.
    Restricted,
    /// `2`: nobody may.
    Disabled,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl IoUring {
    fn from_sysctl(text: Option<&str>) -> Self {
        match text.map(str::trim) {
            Some("1") => IoUring::Restricted,
            Some("2") => IoUring::Disabled,
            _ => IoUring::Enabled,
        }
    }
}

/// Refuse a QEMU that aborts under Lima's `user-v2` network.
///
/// From 10.2 QEMU's main loop watches file descriptors with io_uring when it
/// can. Lima hands QEMU the `user-v2` network as a `-netdev socket`, whose
/// receive handler then gets called with nothing to read: `recv()` fails with
/// `EAGAIN`, and the handler passes the `-1` on to `net_fill_rstate()`, which
/// asserts. VM-to-VM traffic kills a node within minutes (#1495). With
/// io_uring restricted QEMU falls back to `ppoll(2)` and the crash goes away.
/// A banner we can't read passes: the check is for a known crash, not a
/// whitelist.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn check_qemu(command: &str, version: Option<QemuVersion>, io_uring: IoUring) -> Result<()> {
    let Some(version) = version else {
        return Ok(());
    };
    if version < QemuVersion::IO_URING_MAIN_LOOP || io_uring != IoUring::Enabled {
        return Ok(());
    }
    bail!(
        "{command} {version} aborts under Lima's user-v2 network while io_uring is enabled \
         (https://github.com/reliaburger/reliaburger/issues/1495); \
         run `sudo sysctl -w kernel.io_uring_disabled=1` to keep io_uring for root only \
         (add `kernel.io_uring_disabled = 1` to /etc/sysctl.d/ to keep it after a reboot), \
         or install QEMU 10.1 or older, then rerun"
    )
}

/// Check memory and disk for VMs about to start; running owned VMs aren't charged twice.
pub async fn resources(to_start: usize, to_create: usize, root: &Path) -> Result<()> {
    if to_start == 0 {
        return Ok(());
    }
    let memory_level = macos_memory_level().await?;
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let available = match memory_level {
            Some(level) => level_to_bytes(system.total_memory(), level),
            None => system.available_memory(),
        };
        validate_capacity(to_start, to_create, available, free_disk(&root)?)
    })
    .await?
}

/// The percentage of memory macOS considers free before it comes under
/// pressure (`memory_pressure` prints the same figure).
///
/// sysinfo 0.33's macOS "available" subtracted the pages holding compressed
/// memory from free and inactive pages, which never held them: on a busy Mac
/// it reported almost nothing while the kernel reported 60% free. Since 0.38
/// it's active + inactive + free, which counts pages running apps are using
/// and overstates what a VM can take. The kernel's own level is the figure
/// `memory_pressure` prints. Linux's MemAvailable is fine.
async fn macos_memory_level() -> Result<Option<u64>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.memorystatus_level"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        bail!("could not read the macOS memory pressure level");
    }
    Ok(Some(parse_memory_level(&String::from_utf8_lossy(
        &output.stdout,
    ))?))
}

fn parse_memory_level(text: &str) -> Result<u64> {
    let level: u64 = text
        .trim()
        .parse()
        .context("invalid macOS memory pressure level")?;
    if level > 100 {
        bail!("invalid macOS memory pressure level");
    }
    Ok(level)
}

fn level_to_bytes(total: u64, level: u64) -> u64 {
    total / 100 * level
}

fn validate_capacity(
    to_start: usize,
    to_create: usize,
    available_memory: u64,
    free_disk: u64,
) -> Result<()> {
    if to_start == 0 {
        return Ok(());
    }
    let memory_gib = 2 * to_start as u64 + 2;
    let disk_gib = if to_create == 0 {
        0
    } else {
        4 * to_create as u64 + 3
    };
    if available_memory < memory_gib * GIB {
        bail!(
            "starting {to_start} VM(s) needs {memory_gib} GiB available memory; close other VMs or use --nodes 1 for a new cluster"
        );
    }
    if free_disk < disk_gib * GIB {
        bail!("starting {to_start} VM(s) needs {disk_gib} GiB free disk space");
    }
    Ok(())
}

/// Bytes free to an unprivileged user on the filesystem holding `root`.
pub fn free_disk(root: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        let stats = nix::sys::statvfs::statvfs(root)?;
        // Darwin exposes a 32-bit block count; Linux exposes a 64-bit count.
        #[allow(clippy::useless_conversion)]
        let blocks = u64::from(stats.blocks_available());
        Ok(blocks.saturating_mul(stats.fragment_size()))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        bail!("unsupported host filesystem")
    }
}

/// Check loopback listeners before allocating a stopped or missing VM.
pub async fn ports(ports: &[u16]) -> Result<()> {
    let mut listeners = Vec::new();
    for port in ports {
        listeners.push(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).await
            .with_context(|| format!("host port {port} is occupied; choose different --api-port/--ingress-port values for a new cluster"))?);
    }
    Ok(())
}

/// Refuse paths that Lima cannot use for its SSH control sockets.
pub fn socket_paths(root: &Path, names: impl Iterator<Item = impl AsRef<str>>) -> Result<()> {
    let maximum = if cfg!(target_os = "macos") { 104 } else { 108 };
    for name in names {
        let socket = root
            .join("lima")
            .join(name.as_ref())
            .join("ssh.sock.1234567890123456");
        if socket.as_os_str().len() >= maximum {
            bail!(
                "RELIABURGER_HOME is too long for VM control sockets; choose a shorter absolute directory"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_version_is_read_from_distribution_banners() {
        let version = |major, minor, micro| QemuVersion {
            major,
            minor,
            micro,
        };
        let parse = QemuVersion::parse;
        assert_eq!(
            parse("QEMU emulator version 11.1.3 (qemu-11.1.3-1.fc45)\nCopyright (c) 2003-2026"),
            Some(version(11, 1, 3))
        );
        assert_eq!(
            parse("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.4)"),
            Some(version(8, 2, 2))
        );
        assert_eq!(
            parse("QEMU emulator version 11.1.2\n"),
            Some(version(11, 1, 2))
        );
        // A build from git reports the next release's development number.
        assert_eq!(
            parse("QEMU emulator version 11.1.50 (v11.1.0-421-gabcdef)"),
            Some(version(11, 1, 50))
        );
        assert_eq!(parse("QEMU emulator version 10.2"), Some(version(10, 2, 0)));
        assert_eq!(parse(""), None);
        assert_eq!(parse("qemu-system-x86_64: command not found"), None);
        assert_eq!(parse("QEMU emulator version eleven"), None);
    }

    #[test]
    fn io_uring_setting_reads_the_kernel_sysctl() {
        assert_eq!(IoUring::from_sysctl(Some("0\n")), IoUring::Enabled);
        assert_eq!(IoUring::from_sysctl(Some("1\n")), IoUring::Restricted);
        assert_eq!(IoUring::from_sysctl(Some("2\n")), IoUring::Disabled);
        // Kernels before 6.6 have no switch, so io_uring is on.
        assert_eq!(IoUring::from_sysctl(None), IoUring::Enabled);
        assert_eq!(IoUring::from_sysctl(Some("garbage")), IoUring::Enabled);
    }

    #[test]
    fn qemu_with_an_io_uring_main_loop_is_refused_while_io_uring_is_on() {
        let qemu = |text: &str| QemuVersion::parse(text).unwrap();
        let affected = qemu("QEMU emulator version 11.1.3 (qemu-11.1.3-1.fc45)");
        let error = check_qemu("qemu-system-x86_64", Some(affected), IoUring::Enabled)
            .unwrap_err()
            .to_string();
        assert!(error.contains("11.1.3"), "{error}");
        assert!(error.contains("io_uring_disabled"), "{error}");
        assert!(error.contains("issues/1495"), "{error}");

        let first = qemu("QEMU emulator version 10.2.0");
        assert!(check_qemu("qemu-system-aarch64", Some(first), IoUring::Enabled).is_err());
        let later = qemu("QEMU emulator version 11.2.0");
        assert!(check_qemu("qemu-system-aarch64", Some(later), IoUring::Enabled).is_err());
    }

    #[test]
    fn qemu_is_accepted_when_its_main_loop_cannot_use_io_uring() {
        let qemu = |text: &str| QemuVersion::parse(text).unwrap();
        let affected = qemu("QEMU emulator version 11.1.3");
        assert!(check_qemu("qemu-system-x86_64", Some(affected), IoUring::Restricted).is_ok());
        assert!(check_qemu("qemu-system-x86_64", Some(affected), IoUring::Disabled).is_ok());
        // Before 10.2 the main loop always polled with ppoll(2).
        let older = qemu("QEMU emulator version 10.1.5 (Debian 1:10.1.5+ds-1)");
        assert!(check_qemu("qemu-system-x86_64", Some(older), IoUring::Enabled).is_ok());
        let ubuntu = qemu("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.4)");
        assert!(check_qemu("qemu-system-x86_64", Some(ubuntu), IoUring::Enabled).is_ok());
        // A banner we can't read isn't a reason to stop someone.
        assert!(check_qemu("qemu-system-x86_64", None, IoUring::Enabled).is_ok());
    }

    #[test]
    fn capacity_checks_only_charge_for_vms_that_need_to_start() {
        assert!(validate_capacity(3, 3, 8 * GIB, 15 * GIB).is_ok());
        assert!(validate_capacity(3, 3, 7 * GIB, 15 * GIB).is_err());
        assert!(validate_capacity(3, 3, 8 * GIB, 14 * GIB).is_err());
        assert!(validate_capacity(0, 0, 0, 0).is_ok());
        assert!(validate_capacity(3, 0, 8 * GIB, 0).is_ok());
    }

    #[test]
    fn macos_memory_follows_the_kernel_pressure_level() {
        assert_eq!(parse_memory_level("60\n").unwrap(), 60);
        assert!(parse_memory_level("101").is_err());
        assert!(parse_memory_level("").is_err());
        // A 32 GiB Mac at 60% free has room for three VMs.
        let available = level_to_bytes(32 * GIB, 60);
        assert!(available > 19 * GIB && available < 20 * GIB);
        assert!(validate_capacity(3, 3, available, 15 * GIB).is_ok());
        assert!(validate_capacity(3, 3, level_to_bytes(8 * GIB, 60), 15 * GIB).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_reports_a_memory_pressure_level() {
        assert!(macos_memory_level().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn occupied_port_is_reported_before_starting_a_vm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            ports(&[port])
                .await
                .unwrap_err()
                .to_string()
                .contains(&port.to_string())
        );
        drop(listener);
        // A released ephemeral port can be handed straight to another test's
        // outgoing connection, so allow a few fresh ports before failing.
        let mut last = None;
        for _ in 0..5 {
            let free = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            match ports(&[free]).await {
                Ok(()) => return,
                Err(error) => last = Some(error),
            }
        }
        panic!("no free port was accepted: {last:?}");
    }
}

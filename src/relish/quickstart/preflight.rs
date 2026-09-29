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
    {
        let command = format!("qemu-system-{}", std::env::consts::ARCH);
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&command)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await?
        .with_context(|| format!("install {command} before running quickstart"))?;
        if !status.success() {
            bail!("{command} did not pass its version check");
        }
    }
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
/// sysinfo's macOS "available" subtracts the pages holding compressed memory
/// from free and inactive pages, which never held them. On a busy Mac with a
/// few GiB compressed it reports almost nothing available while the kernel
/// reports 60% free, and we'd refuse to start. Linux's MemAvailable is fine.
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

fn free_disk(root: &Path) -> Result<u64> {
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

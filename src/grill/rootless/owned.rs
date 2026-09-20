//! Namespace pinning and startup gates for independently owned rootless helpers.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};

use crate::grill::process_owner::{self, OwnerPhase};

fn pause(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "rootless owner request timed out",
        ));
    }
    std::thread::sleep(Duration::from_millis(5));
    Ok(())
}

// This helper runs before Tokio. Nonblocking connect/read/write share one
// deadline, including a full Unix listen backlog and partial responses.
fn launcher_pid(directory: &Path) -> io::Result<u32> {
    process_owner::validate_socket_directory(directory)?;
    let record = process_owner::load(directory)?;
    let OwnerPhase::Running { pid } = record.phase else {
        return Err(io::Error::other("rootless launcher is not running"));
    };
    let path = process_owner::socket_path(directory, &record);
    process_owner::validate_socket_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("missing owner socket directory"))?,
    )?;
    let address = UnixAddr::new(&path).map_err(io::Error::other)?;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(io::Error::other)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match connect(fd.as_raw_fd(), &address) {
            Ok(()) | Err(nix::errno::Errno::EISCONN) => break,
            Err(
                nix::errno::Errno::EAGAIN
                | nix::errno::Errno::EINPROGRESS
                | nix::errno::Errno::EALREADY
                | nix::errno::Errno::EINTR,
            ) => pause(deadline)?,
            Err(error) => return Err(io::Error::other(error)),
        }
    }
    let mut stream = UnixStream::from(fd);
    let request = format!(
        "{}\n",
        serde_json::json!({"nonce": record.nonce, "action": "status"})
    );
    let mut remaining = request.as_bytes();
    while !remaining.is_empty() {
        match stream.write(remaining) {
            Ok(0) => return Err(io::Error::other("launcher owner closed its socket")),
            Ok(count) => remaining = &remaining[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                pause(deadline)?
            }
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "rootless owner request timed out",
            ));
        }
    }
    let mut bytes = Vec::new();
    while bytes.last() != Some(&b'\n') {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Err(io::Error::other("launcher owner closed its response")),
            Ok(_) => bytes.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                pause(deadline)?
            }
            Err(error) => return Err(error),
        }
        if bytes.len() > 4096 {
            return Err(io::Error::other("oversized launcher owner response"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "rootless owner request timed out",
            ));
        }
    }
    let response: serde_json::Value = serde_json::from_slice(&bytes)?;
    if response.get("error").is_some()
        || response["phase"]["state"] != "running"
        || response["phase"]["pid"].as_u64() != Some(u64::from(pid))
    {
        return Err(io::Error::other(
            "launcher owner did not confirm its running identity",
        ));
    }
    Ok(pid)
}

/// Pin the verified container namespaces, then replace this process with slirp.
/// Only Bun's hidden command calls this, before starting any runtime threads.
pub fn run_helper(launcher: &Path, container_pid: u32, api_socket: &Path) -> io::Result<()> {
    process_owner::validate_socket_directory(
        api_socket
            .parent()
            .ok_or_else(|| io::Error::other("missing rootless API directory"))?,
    )?;
    // A proc directory FD stays tied to this process even after its numeric PID
    // is recycled. Every subsequent lookup is relative to that held directory.
    let process = File::open(format!("/proc/{container_pid}"))?;
    let base = format!("/proc/self/fd/{}", process.as_raw_fd());
    let status = std::fs::read_to_string(format!("{base}/status"))?;
    let parent = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse::<u32>().ok());
    let nested_init = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .is_some_and(|value| {
            let ids: Vec<_> = value.split_whitespace().collect();
            ids.len() >= 2 && ids.last() == Some(&"1")
        });
    let user = File::open(format!("{base}/ns/user"))?;
    let network = File::open(format!("{base}/ns/net"))?;
    if !nested_init
        || parent != Some(launcher_pid(launcher)?)
        || user.metadata()?.ino() == std::fs::metadata("/proc/self/ns/user")?.ino()
        || network.metadata()?.ino() == std::fs::metadata("/proc/self/ns/net")?.ino()
    {
        return Err(io::Error::other(
            "container namespaces do not belong to the owned launcher",
        ));
    }
    for namespace in [&user, &network] {
        fcntl(namespace.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::empty()))
            .map_err(io::Error::other)?;
    }
    let error = std::process::Command::new("slirp4netns")
        .args([
            "--configure",
            "--mtu=65520",
            "--disable-host-loopback",
            "--netns-type=path",
        ])
        .arg(format!("--userns-path=/proc/self/fd/{}", user.as_raw_fd()))
        .arg("--api-socket")
        .arg(api_socket)
        .arg(format!("/proc/self/fd/{}", network.as_raw_fd()))
        .arg("tap0")
        .exec();
    Err(error)
}

/// Hold OCI creation before the payload starts until its network is ready.
/// Runc supplies the init PID through the hook's standard input.
pub fn run_hook(directory: &Path, instance: &str) -> io::Result<()> {
    process_owner::validate_socket_directory(directory)?;
    let mut bytes = Vec::new();
    std::io::stdin().take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("oversized OCI hook input"));
    }
    let state: serde_json::Value = serde_json::from_slice(&bytes)?;
    let pid = state["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("OCI hook omitted its init PID"))?;
    if state["id"].as_str() != Some(instance) {
        return Err(io::Error::other("OCI hook instance mismatch"));
    }
    crate::sesame::identity::atomic_write_mode(
        &directory.join("init.json"),
        &serde_json::to_vec(&pid)?,
        Some(0o600),
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(directory.join("ready"))
        {
            Ok(file) => {
                if !file.metadata()?.is_file() {
                    return Err(io::Error::other("invalid rootless ready marker"));
                }
                let mut bytes = Vec::new();
                file.take(6).read_to_end(&mut bytes)?;
                if bytes != b"ready" {
                    return Err(io::Error::other("invalid rootless ready marker"));
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => pause(deadline)?,
            Err(error) => return Err(error),
        }
    }
}

//! Start the three servers, run them for `--for`, then stop.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::Mutex;

use super::artefacts::{self, Artefacts};
use super::dhcp::{self, DhcpContext, ListenPort};
use super::http::{self, HttpState};
use super::installed::{InstalledRecord, RECORD_FILE, chain_script};
use super::interface::{self, Interface, udp_socket};
use super::tftp::{self, Outcome, TftpFiles, TftpSettings};
use super::{Log, MacAddress, NetbootError, NetbootOptions};

/// How long to listen for another ProxyDHCP before starting.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// Check the artefacts, make sure no other netboot server is answering,
/// then serve until `options.duration` passes or Ctrl-C.
pub async fn run(options: NetbootOptions) -> Result<(), NetbootError> {
    let interface = interface::find(&options.interface)?;
    let artefacts = Arc::new(check_artefacts(&options).await?);
    refuse_if_another_server_answers(&interface).await?;

    let server = interface.address;
    let any = Ipv4Addr::UNSPECIFIED;
    let dhcp_socket = udp_socket(
        "DHCP",
        SocketAddrV4::new(any, dhcp::DHCP_SERVER_PORT),
        Some(&interface),
    )?;
    let proxy_socket = udp_socket(
        "ProxyDHCP",
        SocketAddrV4::new(any, dhcp::PROXY_DHCP_PORT),
        Some(&interface),
    )?;
    let tftp_socket = udp_socket("TFTP", SocketAddrV4::new(server, tftp::TFTP_PORT), None)?;
    let listener = TcpListener::bind(SocketAddrV4::new(server, options.http_port))
        .await
        .map_err(|e| interface::bind_error("HTTP", options.http_port, e))?;
    let record = InstalledRecord::load(&options.directory.join(RECORD_FILE))?;

    print_summary(&options, &interface, &artefacts, &record);
    let log: Log = Arc::new(|line| println!("{line}"));
    let context = Arc::new(DhcpContext {
        server,
        http_port: options.http_port,
        architectures: artefacts.architectures.keys().copied().collect(),
        allowed: options.allowed.clone(),
    });
    let files = Arc::new(tftp_files(&artefacts, server, options.http_port));
    let state = Arc::new(HttpState {
        artefacts,
        server,
        port: options.http_port,
        installed: Mutex::new(record),
        allowed: options.allowed.clone(),
        reinstall: options.reinstall,
        log: log.clone(),
    });

    let dhcp_task = dhcp_loop(dhcp_socket, ListenPort::Dhcp, context.clone(), log.clone());
    let proxy_task = dhcp_loop(proxy_socket, ListenPort::ProxyDhcp, context, log.clone());
    let tftp_log = log.clone();
    let tftp_task = tftp::serve(
        tftp_socket,
        files,
        TftpSettings::default(),
        move |peer, name, outcome| log_transfer(&tftp_log, peer, name, outcome),
    );
    let http_task = axum::serve(listener, http::router(state));

    let stopped = tokio::select! {
        _ = tokio::signal::ctrl_c() => "stopped".to_string(),
        _ = tokio::time::sleep(options.duration) => {
            format!("stopping after {} (--for)", describe_duration(options.duration))
        }
        result = dhcp_task => failed("DHCP", result),
        result = proxy_task => failed("ProxyDHCP", result),
        result = tftp_task => failed("TFTP", result),
        result = http_task => failed("HTTP", result),
    };
    println!("relish netboot: {stopped}");
    Ok(())
}

async fn check_artefacts(options: &NetbootOptions) -> Result<Artefacts, NetbootError> {
    let directory = options.directory.clone();
    let keys = options.keys.clone();
    println!(
        "relish netboot: checking {} against the trusted keys",
        directory.display()
    );
    // Hashing a 2 GB disk image takes seconds: keep it off the runtime.
    tokio::task::spawn_blocking(move || {
        artefacts::load(&directory, &keys, |line| println!("  {line}"))
    })
    .await
    .map_err(|e| NetbootError::io("checking the artefacts", std::io::Error::other(e)))?
}

/// Broadcast a PXE DHCPDISCOVER and listen for a boot server's offer. Two
/// ProxyDHCPs on one LAN race for every machine, so refuse to be the second.
async fn refuse_if_another_server_answers(interface: &Interface) -> Result<(), NetbootError> {
    let address = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, dhcp::DHCP_CLIENT_PORT);
    let socket = match interface::udp_socket("the DHCP client port", address, Some(interface)) {
        Ok(socket) => socket,
        Err(NetbootError::NeedsRoot) => return Err(NetbootError::NeedsRoot),
        Err(error) => {
            eprintln!(
                "relish netboot: warning: can't check for another netboot server ({error}); make sure there isn't one"
            );
            return Ok(());
        }
    };
    let xid: u32 = rand::random();
    // A locally administered address, so it's never a real machine's.
    let mut mac = rand::random::<[u8; 6]>();
    mac[0] = (mac[0] & 0xfe) | 0x02;
    let probe = dhcp::probe_discover(xid, MacAddress(mac));
    let Some(bytes) = dhcp::encode(&probe) else {
        return Ok(());
    };
    let broadcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, dhcp::DHCP_SERVER_PORT);
    if let Err(error) = socket.send_to(&bytes, broadcast).await {
        eprintln!(
            "relish netboot: warning: can't check for another netboot server ({error}); make sure there isn't one"
        );
        return Ok(());
    }
    let listen = listen_for_competitor(&socket, xid, interface.address);
    match tokio::time::timeout(PROBE_WAIT, listen).await {
        Ok(Some(server)) => Err(NetbootError::CompetingServer { server }),
        _ => Ok(()),
    }
}

async fn listen_for_competitor(socket: &UdpSocket, xid: u32, own: Ipv4Addr) -> Option<Ipv4Addr> {
    let mut buffer = vec![0u8; 1500];
    loop {
        let (len, source) = socket.recv_from(&mut buffer).await.ok()?;
        let SocketAddr::V4(source) = source else {
            continue;
        };
        let Some(reply) = dhcp::decode(&buffer[..len]) else {
            continue;
        };
        if let Some(server) = dhcp::competing_server(&reply, xid, *source.ip())
            && server != own
        {
            return Some(server);
        }
    }
}

async fn dhcp_loop(
    socket: UdpSocket,
    port: ListenPort,
    context: Arc<DhcpContext>,
    log: Log,
) -> std::io::Result<()> {
    dhcp::serve(Arc::new(socket), port, context, move |line| log(line)).await
}

/// What TFTP serves: iPXE for each architecture (the SNP build, under
/// both the name DHCP gives out and its own) and the chain script.
fn tftp_files(artefacts: &Artefacts, server: Ipv4Addr, http_port: u16) -> TftpFiles {
    let mut files = TftpFiles::new();
    for (arch, release) in &artefacts.architectures {
        files.insert(arch.boot_file(), release.ipxe.clone());
        files.insert(arch.snp_file(), release.ipxe.clone());
    }
    files.insert(
        "boot.ipxe".to_string(),
        chain_script(server, http_port).into_bytes().into(),
    );
    files
}

fn log_transfer(log: &Log, peer: SocketAddr, name: &str, outcome: Outcome) {
    match outcome {
        Outcome::Sent { bytes, block_size } => log(format!(
            "tftp: {peer}: {name}, {bytes} bytes in {block_size}-byte blocks"
        )),
        // UEFI firmware asks for the size first, then for the file.
        Outcome::Probed => {}
        Outcome::Aborted { message } => log(format!(
            "tftp: {peer}: {name}: the client stopped ({message})"
        )),
        Outcome::TimedOut => log(format!("tftp: {peer}: {name}: no acknowledgement, gave up")),
    }
}

fn failed<E: std::fmt::Display>(what: &str, result: Result<(), E>) -> String {
    match result {
        Ok(()) => format!("the {what} server stopped"),
        Err(error) => format!("the {what} server failed: {error}"),
    }
}

fn describe_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn print_summary(
    options: &NetbootOptions,
    interface: &Interface,
    artefacts: &Artefacts,
    record: &InstalledRecord,
) {
    let server = interface.address;
    let port = options.http_port;
    println!(
        "relish netboot: serving on {} ({server}) for {}; Ctrl-C stops it",
        interface.name,
        describe_duration(options.duration)
    );
    for (arch, release) in &artefacts.architectures {
        println!(
            "  {arch}: OS {}, {} files checked; PXE boots {} (TFTP), then http://{server}:{port}/{}/installer.efi",
            release.version,
            release.files.len(),
            arch.boot_file(),
            arch.ipxe_name(),
        );
    }
    if options.allowed.is_empty() {
        println!("  answering every PXE client on the network");
    } else {
        let macs: Vec<String> = options.allowed.iter().map(|m| m.to_string()).collect();
        println!("  answering only {}", macs.join(", "));
    }
    let installed = record.machines().len();
    if options.reinstall {
        println!("  --reinstall: machines that installed already install again");
    } else if installed > 0 {
        println!(
            "  {installed} machine(s) installed already get exit (listed in {}; --reinstall to ignore it)",
            options.directory.join(RECORD_FILE).display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::netboot::Arch;
    use crate::relish::netboot::artefacts::fixture;

    #[test]
    fn tftp_serves_the_snp_ipxe_under_both_names_and_the_chain_script() {
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = fixture::write(
            dir.path(),
            "aarch64",
            &fixture::borrowed(&fixture::release("arm64")),
        );
        let artefacts = artefacts::load(dir.path(), &[key], |_| {}).unwrap();
        let files = tftp_files(&artefacts, Ipv4Addr::new(10, 0, 0, 5), 8081);
        let mut names: Vec<&String> = files.keys().collect();
        names.sort();
        assert_eq!(names, ["boot.ipxe", "ipxe-arm64.efi", "ipxe-snp-arm64.efi"]);
        let snp = &artefacts.architectures[&Arch::Arm64].ipxe;
        assert_eq!(&files["ipxe-arm64.efi"], snp);
        let script = String::from_utf8(files["boot.ipxe"].to_vec()).unwrap();
        assert!(
            script.contains("http://10.0.0.5:8081/boot.ipxe?"),
            "{script}"
        );
    }

    #[test]
    fn durations_print_in_their_largest_whole_unit() {
        assert_eq!(describe_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(describe_duration(Duration::from_secs(5400)), "90m");
        assert_eq!(describe_duration(Duration::from_secs(172_800)), "2d");
        assert_eq!(describe_duration(Duration::from_secs(61)), "61s");
    }
}

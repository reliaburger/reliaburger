//! Start the three servers, run them for `--for`, then stop.

use std::io::IsTerminal;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Mutex, mpsc};

use super::artefacts::{self, Artefacts};
use super::dhcp::{self, DhcpContext, ListenPort};
use super::http::{self, HttpState};
use super::installed::{InstalledRecord, RECORD_FILE, chain_script};
use super::interface::{self, Interface, udp_socket};
use super::tftp::{self, Outcome, TftpFiles, TftpSettings};
use super::wipe::{self, DiskSession, Question};
use super::{IpxeBuild, Log, MacAddress, NetbootError, NetbootOptions};

/// How long to listen for the LAN's DHCP server and another ProxyDHCP
/// before starting.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// Check the artefacts, make sure no other netboot server is answering,
/// then serve until `options.duration` passes or Ctrl-C.
pub async fn run(options: NetbootOptions) -> Result<(), NetbootError> {
    let interface = interface::find(&options.interface)?;
    let artefacts = Arc::new(check_artefacts(&options).await?);
    let server = interface.address;
    let files = Arc::new(tftp_files(
        &artefacts,
        options.ipxe,
        &options.directory,
        server,
        options.http_port,
    )?);
    check_the_network(&interface).await?;

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
    let state = Arc::new(HttpState {
        artefacts,
        server,
        port: options.http_port,
        installed: Mutex::new(record),
        allowed: options.allowed.clone(),
        reinstall: options.reinstall,
        wipe: options.wipe.clone(),
        operator: operator(),
        question_timeout: wipe::QUESTION_TIMEOUT,
        disks: Mutex::new(DiskSession::default()),
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

/// When stdin is a terminal, a way to ask the operator about used disks:
/// a thread reads stdin a line at a time (a blocking read can't be
/// cancelled, so it gets a thread of its own, as tokio's docs advise for
/// interactive input), and [`wipe::ask_operator`] asks one question at a
/// time. Without a terminal there's nobody to ask: `None`.
fn operator() -> Option<mpsc::UnboundedSender<Question>> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let (send_line, lines) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if send_line.send(line).is_err() {
                break;
            }
        }
    });
    let (ask, questions) = mpsc::unbounded_channel();
    tokio::spawn(wipe::ask_operator(questions, lines, |line| {
        println!("relish netboot: {line}")
    }));
    Some(ask)
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

/// What answered the start-up probe.
#[derive(Debug, PartialEq, Eq)]
enum Network {
    /// Another PXE boot server: the two would race for every machine.
    BootServer(Ipv4Addr),
    /// A DHCP server handing out addresses, and no other boot server:
    /// exactly what a ProxyDHCP needs beside it.
    AddressServer(Ipv4Addr),
    /// Nothing at all.
    Silent,
}

/// Broadcast a PXE DHCPDISCOVER on `interface` and act on what answers:
/// refuse to start beside another boot server, and warn when nothing hands
/// out addresses, since the machines we boot need one before they ask us
/// anything.
async fn check_the_network(interface: &Interface) -> Result<(), NetbootError> {
    let skipped = |error: &dyn std::fmt::Display| {
        eprintln!(
            "relish netboot: warning: can't check the network ({error}); make sure no other netboot server runs on it, and that its DHCP server is up"
        );
    };
    let socket = match interface::probe_socket(dhcp::DHCP_CLIENT_PORT, Some(interface)) {
        Ok(socket) => socket,
        Err(NetbootError::NeedsRoot) => return Err(NetbootError::NeedsRoot),
        Err(error) => {
            skipped(&error);
            return Ok(());
        }
    };
    let broadcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, dhcp::DHCP_SERVER_PORT);
    match probe(&socket, broadcast, interface.address, PROBE_WAIT).await {
        Ok(Network::Silent) => eprintln!("{}", verdict(Network::Silent, interface)?),
        Ok(network) => println!("{}", verdict(network, interface)?),
        Err(error) => skipped(&error),
    }
    Ok(())
}

/// Send a PXE-looking DHCPDISCOVER from `socket` to `target`, then listen
/// for `wait`. Another boot server's offer ends the wait at once. Offers
/// from `own` address (this machine, already serving) and for other
/// transactions don't count.
async fn probe(
    socket: &UdpSocket,
    target: SocketAddrV4,
    own: Ipv4Addr,
    wait: Duration,
) -> std::io::Result<Network> {
    let xid: u32 = rand::random();
    // A locally administered address, so it's never a real machine's.
    let mut mac = rand::random::<[u8; 6]>();
    mac[0] = (mac[0] & 0xfe) | 0x02;
    let discover = dhcp::probe_discover(xid, MacAddress(mac));
    let bytes =
        dhcp::encode(&discover).ok_or_else(|| std::io::Error::other("can't encode the probe"))?;
    socket.send_to(&bytes, target).await?;

    let mut address_server = None;
    let mut buffer = vec![0u8; 1500];
    let deadline = tokio::time::Instant::now() + wait;
    // `timeout_at` gives up at the deadline: `Err` means the wait is over.
    while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
    {
        let (len, source) = received?;
        let SocketAddr::V4(source) = source else {
            continue;
        };
        let Some(reply) = dhcp::decode(&buffer[..len]) else {
            continue;
        };
        if let Some(server) = dhcp::competing_server(&reply, xid, *source.ip())
            && server != own
        {
            return Ok(Network::BootServer(server));
        }
        if let Some(server) = dhcp::address_server(&reply, xid, *source.ip()) {
            address_server.get_or_insert(server);
        }
    }
    Ok(address_server.map_or(Network::Silent, Network::AddressServer))
}

/// The line to print about what the probe found, or the refusal to start.
fn verdict(network: Network, interface: &Interface) -> Result<String, NetbootError> {
    let name = &interface.name;
    match network {
        Network::BootServer(server) => Err(NetbootError::CompetingServer { server }),
        Network::AddressServer(server) => Ok(format!(
            "relish netboot: {server} hands out addresses on {name}, and no other netboot server answers"
        )),
        Network::Silent => Ok(format!(
            "relish netboot: warning: nothing hands out addresses on {name}; no DHCP server answered within {}s. \
             Machines that network-boot need an address before they ask relish anything. \
             Is the router (the Pi, in the lab) up, and on the same switch? Serving anyway",
            PROBE_WAIT.as_secs()
        )),
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

/// What TFTP serves: iPXE for each architecture and the chain script.
/// DHCP names `ipxe-<arch>.efi`, so that name gets the `build` chosen
/// (`--ipxe`); the SNP build is also there under its own name. `directory`
/// is the served directory, for naming a full build that isn't there.
fn tftp_files(
    artefacts: &Artefacts,
    build: IpxeBuild,
    directory: &Path,
    server: Ipv4Addr,
    http_port: u16,
) -> Result<TftpFiles, NetbootError> {
    let mut files = TftpFiles::new();
    for (arch, release) in &artefacts.architectures {
        let chosen = release
            .ipxe_build(build)
            .ok_or_else(|| NetbootError::IpxeMissing {
                file: directory
                    .join(arch.directory())
                    .join("netboot")
                    .join(build.file(*arch)),
            })?;
        files.insert(arch.boot_file(), chosen.clone());
        files.insert(arch.snp_file(), release.ipxe.clone());
    }
    files.insert(
        "boot.ipxe".to_string(),
        chain_script(server, http_port).into_bytes().into(),
    );
    Ok(files)
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
            "  {arch}: OS {}, {} files checked; PXE boots {} (TFTP, iPXE's {} build), then http://{server}:{port}/{}/installer.efi",
            release.version,
            release.files.len(),
            arch.boot_file(),
            options.ipxe,
            arch.ipxe_name(),
        );
    }
    if options.allowed.is_empty() {
        println!("  answering every PXE client on the network");
    } else {
        let macs: Vec<String> = options.allowed.iter().map(|m| m.to_string()).collect();
        println!("  answering only {}", macs.join(", "));
    }
    if !options.wipe.is_empty() {
        let macs: Vec<String> = options.wipe.iter().map(|m| m.to_string()).collect();
        println!(
            "  --wipe: wiping the disk of {} without asking",
            macs.join(", ")
        );
    }
    if std::io::stdin().is_terminal() {
        println!("  a machine whose disk isn't blank asks here first: answer y to wipe it");
    } else {
        println!(
            "  no terminal to ask on: a disk that isn't blank is left alone unless --wipe lists its machine"
        );
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

    use crate::relish::netboot::dhcp::DhcpContext;
    use dhcproto::v4::{DhcpOption, Message, MessageType, Opcode};

    fn arm64_release(files: &[(String, Vec<u8>)]) -> (tempfile::TempDir, Artefacts) {
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = fixture::write(dir.path(), "aarch64", &fixture::borrowed(files));
        let artefacts = artefacts::load(dir.path(), &[key], |_| {}).unwrap();
        (dir, artefacts)
    }

    #[test]
    fn tftp_serves_the_snp_ipxe_under_both_names_and_the_chain_script() {
        let (dir, artefacts) = arm64_release(&fixture::release("arm64"));
        let files = tftp_files(
            &artefacts,
            IpxeBuild::Snp,
            dir.path(),
            Ipv4Addr::new(10, 0, 0, 5),
            8081,
        )
        .unwrap();
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
    fn ipxe_full_serves_the_full_driver_build_under_the_name_dhcp_gives() {
        let (dir, artefacts) = arm64_release(&fixture::release("arm64"));
        let server = Ipv4Addr::new(10, 0, 0, 5);
        let files = tftp_files(&artefacts, IpxeBuild::Full, dir.path(), server, 8081).unwrap();
        assert_eq!(&files["ipxe-arm64.efi"][..], b"ipxe with drivers");
        let snp = &artefacts.architectures[&Arch::Arm64].ipxe;
        assert_eq!(&files["ipxe-snp-arm64.efi"], snp);
    }

    #[test]
    fn ipxe_full_is_refused_when_the_release_lacks_it() {
        let files: Vec<_> = fixture::release("arm64")
            .into_iter()
            .filter(|(name, _)| name != "ipxe-arm64.efi")
            .collect();
        let (dir, artefacts) = arm64_release(&files);
        let server = Ipv4Addr::new(10, 0, 0, 5);
        let error = tftp_files(&artefacts, IpxeBuild::Full, dir.path(), server, 8081).unwrap_err();
        let NetbootError::IpxeMissing { ref file } = error else {
            panic!("{error}");
        };
        assert_eq!(file, &dir.path().join("aarch64/netboot/ipxe-arm64.efi"));
        assert!(error.to_string().contains("--ipxe snp"), "{error}");
        assert!(tftp_files(&artefacts, IpxeBuild::Snp, dir.path(), server, 8081).is_ok());
    }

    const PI: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
    const OWN: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
    const OTHER_PROXY: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 3);

    /// The Pi's dnsmasq: an address and nothing about booting.
    fn router_offer(request: &Message) -> Message {
        let mut offer = Message::default();
        offer
            .set_opcode(Opcode::BootReply)
            .set_xid(request.xid())
            .set_yiaddr(Ipv4Addr::new(10, 77, 0, 150))
            .set_chaddr(request.chaddr());
        offer
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Offer));
        offer.opts_mut().insert(DhcpOption::ServerIdentifier(PI));
        offer
    }

    /// A ProxyDHCP at `server`: a boot file and no address.
    fn proxy_offer(request: &Message, server: Ipv4Addr) -> Message {
        let context = DhcpContext {
            server,
            http_port: 8080,
            architectures: [Arch::X86_64].into(),
            allowed: Vec::new(),
        };
        dhcp::answer(request, dhcp::ListenPort::Dhcp, &context)
            .unwrap()
            .reply
    }

    /// A DHCP server on loopback that answers the first request it gets
    /// with whatever `replies` makes of it.
    async fn fake_lan(replies: impl Fn(&Message) -> Vec<Message> + Send + 'static) -> SocketAddrV4 {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let SocketAddr::V4(address) = server.local_addr().unwrap() else {
            unreachable!()
        };
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 1500];
            let (len, from) = server.recv_from(&mut buffer).await.unwrap();
            let request = dhcp::decode(&buffer[..len]).unwrap();
            for reply in replies(&request) {
                let bytes = dhcp::encode(&reply).unwrap();
                server.send_to(&bytes, from).await.unwrap();
            }
        });
        address
    }

    async fn probe_on_loopback(
        replies: impl Fn(&Message) -> Vec<Message> + Send + 'static,
    ) -> Network {
        let lan = fake_lan(replies).await;
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        probe(&socket, lan, OWN, Duration::from_millis(300))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_probe_finds_the_router_handing_out_addresses() {
        let network = probe_on_loopback(|request| vec![router_offer(request)]).await;
        assert_eq!(network, Network::AddressServer(PI));
    }

    #[tokio::test]
    async fn the_probe_finds_another_boot_server_even_beside_a_router() {
        let network = probe_on_loopback(|request| {
            vec![router_offer(request), proxy_offer(request, OTHER_PROXY)]
        })
        .await;
        assert_eq!(network, Network::BootServer(OTHER_PROXY));
    }

    #[tokio::test]
    async fn the_probe_hears_nothing_on_a_silent_link() {
        assert_eq!(probe_on_loopback(|_| Vec::new()).await, Network::Silent);
    }

    #[tokio::test]
    async fn the_probe_ignores_offers_for_someone_else_and_from_ourselves() {
        let network = probe_on_loopback(|request| {
            let mut stranger = router_offer(request);
            stranger.set_xid(request.xid().wrapping_add(1));
            vec![stranger, proxy_offer(request, OWN)]
        })
        .await;
        assert_eq!(network, Network::Silent);
    }

    fn en7() -> Interface {
        Interface {
            name: "en7".into(),
            address: OWN,
            loopback: false,
        }
    }

    #[test]
    fn what_the_probe_found_decides_whether_to_start() {
        let line = verdict(Network::AddressServer(PI), &en7()).unwrap();
        assert!(
            line.contains("10.77.0.1 hands out addresses on en7"),
            "{line}"
        );

        let line = verdict(Network::Silent, &en7()).unwrap();
        assert!(line.contains("warning"), "{line}");
        assert!(
            line.contains("nothing hands out addresses on en7"),
            "{line}"
        );
        assert!(line.contains("router (the Pi"), "{line}");

        assert!(matches!(
            verdict(Network::BootServer(OTHER_PROXY), &en7()),
            Err(NetbootError::CompetingServer { server }) if server == OTHER_PROXY
        ));
    }

    #[test]
    fn durations_print_in_their_largest_whole_unit() {
        assert_eq!(describe_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(describe_duration(Duration::from_secs(5400)), "90m");
        assert_eq!(describe_duration(Duration::from_secs(172_800)), "2d");
        assert_eq!(describe_duration(Duration::from_secs(61)), "61s");
    }
}

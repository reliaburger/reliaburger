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
use super::{DhcpSetup, IpxeBuild, Log, MacAddress, NetbootError, NetbootOptions};

/// How long to listen for the LAN's DHCP server and another ProxyDHCP
/// before starting. dnsmasq, which many home routers run, pings an address
/// for about three seconds before offering it, so two seconds missed it.
const PROBE_WAIT: Duration = Duration::from_secs(5);

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
    check_the_network(&interface, options.dhcp).await?;

    let any = Ipv4Addr::UNSPECIFIED;
    // With the DHCP server on this machine, it owns UDP 67 and sends PXE
    // clients to 4011, so there's nothing for us to hear on 67.
    let dhcp_socket = match options.dhcp {
        DhcpSetup::Router => Some(udp_socket(
            "DHCP",
            SocketAddrV4::new(any, dhcp::DHCP_SERVER_PORT),
            Some(&interface),
        )?),
        DhcpSetup::ThisMachine => None,
    };
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

    let (dhcp_context, dhcp_log) = (context.clone(), log.clone());
    let dhcp_task = async move {
        match dhcp_socket {
            Some(socket) => dhcp_loop(socket, ListenPort::Dhcp, dhcp_context, dhcp_log).await,
            // Never finishes, so the select! below never picks it.
            None => std::future::pending().await,
        }
    };
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
    /// DHCP servers handing out addresses, in the order they answered, and
    /// no other boot server: what a ProxyDHCP needs beside it.
    AddressServers(Vec<AddressServer>),
    /// Nothing at all.
    Silent,
}

/// A DHCP server that offered the probe an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddressServer {
    address: Ipv4Addr,
    /// Its offer carried option 60 `PXEClient`, sending PXE clients to its
    /// UDP 4011 ([`DhcpSetup::ThisMachine`]).
    pxe_client: bool,
}

/// What to print about the network before serving.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    Fine(String),
    Warning(String),
}

/// Broadcast a PXE DHCPDISCOVER on `interface` and act on what answers:
/// refuse to start beside another boot server, and warn when nothing hands
/// out addresses, since the machines we boot need one before they ask us
/// anything. With `setup` [`DhcpSetup::ThisMachine`], the DHCP server must
/// be this machine's alone.
async fn check_the_network(interface: &Interface, setup: DhcpSetup) -> Result<(), NetbootError> {
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
    let network = match probe(&socket, broadcast, interface.address, PROBE_WAIT).await {
        Ok(network) => network,
        Err(error) => {
            skipped(&error);
            return Ok(());
        }
    };
    let report = match setup {
        DhcpSetup::Router => verdict(network, interface)?,
        DhcpSetup::ThisMachine => {
            verdict_beside_local_dhcp(network, interface, local_dhcp_server_runs())?
        }
    };
    match report {
        Report::Fine(line) => println!("{line}"),
        Report::Warning(line) => eprintln!("{line}"),
    }
    Ok(())
}

/// Whether something on this machine holds UDP 67, as a DHCP server does.
/// Our own server socket is exclusive, so binding it fails if anything
/// else has the port.
fn local_dhcp_server_runs() -> bool {
    let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, dhcp::DHCP_SERVER_PORT);
    matches!(
        udp_socket("DHCP", any, None),
        Err(NetbootError::PortInUse { .. })
    )
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

    let mut address_servers: Vec<AddressServer> = Vec::new();
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
        if let Some(address) = dhcp::address_server(&reply, xid, *source.ip())
            && !address_servers.iter().any(|s| s.address == address)
        {
            address_servers.push(AddressServer {
                address,
                pxe_client: dhcp::says_pxe_client(&reply),
            });
        }
    }
    if address_servers.is_empty() {
        Ok(Network::Silent)
    } else {
        Ok(Network::AddressServers(address_servers))
    }
}

/// The line to print about what the probe found beside the LAN's router,
/// or the refusal to start.
fn verdict(network: Network, interface: &Interface) -> Result<Report, NetbootError> {
    let name = &interface.name;
    match network {
        Network::BootServer(server) => Err(NetbootError::CompetingServer { server }),
        Network::AddressServers(servers) => {
            let addresses: Vec<String> = servers.iter().map(|s| s.address.to_string()).collect();
            Ok(Report::Fine(format!(
                "relish netboot: {} hands out addresses on {name}, and no other netboot server answers",
                addresses.join(" and ")
            )))
        }
        Network::Silent => Ok(Report::Warning(format!(
            "relish netboot: warning: nothing hands out addresses on {name}; no DHCP server answered within {}s. \
             Machines that network-boot need an address before they ask relish anything. \
             Is your router up, and on the same switch? Serving anyway",
            PROBE_WAIT.as_secs()
        ))),
    }
}

/// The same for `--mode-dhcp-proxy`, where this machine's own DHCP server
/// hands out the addresses and sends PXE clients to our UDP 4011.
/// `local_dhcp` says whether anything here holds UDP 67: the probe can't
/// always hear a server on its own machine.
fn verdict_beside_local_dhcp(
    network: Network,
    interface: &Interface,
    local_dhcp: bool,
) -> Result<Report, NetbootError> {
    let name = &interface.name;
    let own = interface.address;
    let servers = match network {
        Network::BootServer(server) => return Err(NetbootError::CompetingServer { server }),
        Network::AddressServers(servers) => servers,
        Network::Silent if local_dhcp => {
            return Ok(Report::Fine(format!(
                "relish netboot: a DHCP server on this machine holds UDP 67 (the probe heard no offer on {name}, \
                 which happens when the server answers from the same machine); answering PXE on UDP 4011 only"
            )));
        }
        Network::Silent => {
            return Ok(Report::Warning(format!(
                "relish netboot: warning: --mode-dhcp-proxy, but nothing on this machine holds UDP 67 and nothing answered on {name}. \
                 Start the DHCP server (dnsmasq, image/lab/mac/README.md) before the machines boot. Serving anyway"
            )));
        }
    };
    if let Some(other) = servers.iter().find(|s| s.address != own) {
        return Err(NetbootError::AnotherDhcpServer {
            server: other.address,
            interface: name.clone(),
        });
    }
    if servers.iter().any(|s| s.pxe_client) {
        Ok(Report::Fine(format!(
            "relish netboot: this machine's DHCP server ({own}) hands out addresses on {name} and sends PXE clients to UDP 4011; answering there only"
        )))
    } else {
        Ok(Report::Warning(format!(
            "relish netboot: warning: this machine's DHCP server answers on {name} without option 60 PXEClient, \
             so PXE firmware won't ask relish on UDP 4011. For dnsmasq: dhcp-vendorclass=set:pxe,PXEClient \
             and dhcp-option-force=tag:pxe,60,PXEClient (image/lab/mac/dnsmasq.conf). Serving anyway"
        )))
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
    if options.dhcp == DhcpSetup::ThisMachine {
        println!(
            "  --mode-dhcp-proxy: answering PXE on UDP 4011 only; this machine's DHCP server owns UDP 67"
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

    const ROUTER: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
    const OWN: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
    const OTHER_PROXY: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 3);

    /// A home router's DHCP server: an address and nothing about booting.
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
        offer
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier(ROUTER));
        offer
    }

    /// The lab Mac's dnsmasq: an address from this machine, with option 60
    /// `PXEClient` sending PXE clients to its UDP 4011.
    fn local_dhcp_offer(request: &Message) -> Message {
        let mut offer = router_offer(request);
        offer.opts_mut().insert(DhcpOption::ServerIdentifier(OWN));
        offer
            .opts_mut()
            .insert(DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));
        offer
    }

    fn served_by(address: Ipv4Addr, pxe_client: bool) -> Network {
        Network::AddressServers(vec![AddressServer {
            address,
            pxe_client,
        }])
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
        assert_eq!(network, served_by(ROUTER, false));
    }

    #[tokio::test]
    async fn the_probe_finds_this_machines_dhcp_server_and_its_pxeclient_option() {
        let network = probe_on_loopback(|request| vec![local_dhcp_offer(request)]).await;
        assert_eq!(network, served_by(OWN, true));
    }

    #[tokio::test]
    async fn the_probe_lists_every_dhcp_server_once() {
        let network = probe_on_loopback(|request| {
            vec![
                local_dhcp_offer(request),
                router_offer(request),
                router_offer(request),
            ]
        })
        .await;
        let both = Network::AddressServers(vec![
            AddressServer {
                address: OWN,
                pxe_client: true,
            },
            AddressServer {
                address: ROUTER,
                pxe_client: false,
            },
        ]);
        assert_eq!(network, both);
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
        let Ok(Report::Fine(line)) = verdict(served_by(ROUTER, false), &en7()) else {
            panic!("a router should be fine");
        };
        assert!(
            line.contains("10.77.0.1 hands out addresses on en7"),
            "{line}"
        );

        let Ok(Report::Warning(line)) = verdict(Network::Silent, &en7()) else {
            panic!("silence should warn");
        };
        assert!(line.contains("warning"), "{line}");
        assert!(
            line.contains("nothing hands out addresses on en7"),
            "{line}"
        );
        assert!(line.contains("Is your router up"), "{line}");

        assert!(matches!(
            verdict(Network::BootServer(OTHER_PROXY), &en7()),
            Err(NetbootError::CompetingServer { server }) if server == OTHER_PROXY
        ));
    }

    #[test]
    fn beside_its_own_dhcp_server_relish_wants_option_60_and_no_other_dhcp_server() {
        let Ok(Report::Fine(line)) = verdict_beside_local_dhcp(served_by(OWN, true), &en7(), true)
        else {
            panic!("this machine's dnsmasq with option 60 should be fine");
        };
        assert!(line.contains("sends PXE clients to UDP 4011"), "{line}");

        let Ok(Report::Warning(line)) =
            verdict_beside_local_dhcp(served_by(OWN, false), &en7(), true)
        else {
            panic!("a local DHCP server without option 60 should warn");
        };
        assert!(line.contains("without option 60 PXEClient"), "{line}");
        assert!(
            line.contains("dhcp-option-force=tag:pxe,60,PXEClient"),
            "{line}"
        );

        let both = Network::AddressServers(vec![
            AddressServer {
                address: OWN,
                pxe_client: true,
            },
            AddressServer {
                address: ROUTER,
                pxe_client: false,
            },
        ]);
        let error = verdict_beside_local_dhcp(both, &en7(), true).unwrap_err();
        assert!(
            matches!(&error, NetbootError::AnotherDhcpServer { server, interface }
                if *server == ROUTER && interface == "en7"),
            "{error}"
        );
        assert!(
            error.to_string().contains("Drop --mode-dhcp-proxy"),
            "{error}"
        );

        assert!(matches!(
            verdict_beside_local_dhcp(Network::BootServer(OTHER_PROXY), &en7(), true),
            Err(NetbootError::CompetingServer { server }) if server == OTHER_PROXY
        ));
    }

    #[test]
    fn beside_its_own_dhcp_server_silence_is_fine_only_if_something_holds_port_67() {
        let Ok(Report::Fine(line)) = verdict_beside_local_dhcp(Network::Silent, &en7(), true)
        else {
            panic!("a held UDP 67 should be fine");
        };
        assert!(line.contains("answering PXE on UDP 4011 only"), "{line}");

        let Ok(Report::Warning(line)) = verdict_beside_local_dhcp(Network::Silent, &en7(), false)
        else {
            panic!("nothing on UDP 67 should warn");
        };
        assert!(
            line.contains("nothing on this machine holds UDP 67"),
            "{line}"
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

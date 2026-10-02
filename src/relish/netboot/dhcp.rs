//! The ProxyDHCP: tell PXE firmware what to boot, never what address to use.
//!
//! When a machine network-boots, its firmware broadcasts a DHCPDISCOVER
//! with option 60 set to `PXEClient…`. The LAN's router answers with an
//! address; a ProxyDHCP answers the same broadcast with only the boot
//! part: who the boot server is (`siaddr`, option 54) and which file to
//! fetch (option 67 and the BOOTP `file` field). Its `yiaddr` is always
//! zero, so it can't clash with the router (PXE specification 2.1, section
//! 2.2.4). Some firmware, and iPXE, then confirm with a DHCPREQUEST to the
//! proxy on UDP 4011, which gets the same answer as a DHCPACK.
//!
//! [`answer`] decides the reply from the request and nothing else, so the
//! rules are tested without a network. [`serve`] is the loop around it.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use dhcproto::v4::{
    Architecture, DhcpOption, Flags, HType, Message, MessageType, Opcode, OptionCode,
};
use dhcproto::{Decodable, Decoder, Encodable};
use tokio::net::UdpSocket;

use super::{Arch, MacAddress};

/// Where DHCP servers listen.
pub const DHCP_SERVER_PORT: u16 = 67;
/// Where DHCP clients listen.
pub const DHCP_CLIENT_PORT: u16 = 68;
/// Where PXE clients send a ProxyDHCP their DHCPREQUEST.
pub const PROXY_DHCP_PORT: u16 = 4011;

/// BOOTP messages are at least 300 bytes (RFC 1542, section 2.1); some PXE
/// ROMs drop shorter ones.
const MIN_MESSAGE_LEN: usize = 300;

/// PXE vendor option 6, `PXE_DISCOVERY_CONTROL`: bit 3 means "the offer
/// names a boot file: download it, no menu, no boot server discovery".
const DISCOVERY_CONTROL: [u8; 3] = [6, 1, 8];
/// PXE vendor option 71, `PXE_BOOT_ITEM`, which a 4011 reply echoes.
const BOOT_ITEM: u8 = 71;

/// Which port a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenPort {
    /// UDP 67, broadcasts from firmware looking for an address and a boot file.
    Dhcp,
    /// UDP 4011, requests sent to the ProxyDHCP itself.
    ProxyDhcp,
}

/// What the ProxyDHCP knows about itself.
#[derive(Debug, Clone)]
pub struct DhcpContext {
    /// This server's address on the LAN.
    pub server: Ipv4Addr,
    /// The HTTP server's port, for HTTP Boot URLs.
    pub http_port: u16,
    /// The architectures with checked artefacts.
    pub architectures: BTreeSet<Arch>,
    /// When not empty, only these machines are answered.
    pub allowed: Vec<MacAddress>,
}

/// The file a reply tells the client to boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootFile {
    /// iPXE over TFTP, for PXE firmware.
    Ipxe(Arch),
    /// iPXE over HTTP, for UEFI HTTP Boot firmware.
    HttpIpxe(Arch),
    /// `boot.ipxe` over TFTP, for iPXE asking for its script.
    Script,
}

impl BootFile {
    /// The name or URL as it goes in the reply.
    pub fn name(&self, context: &DhcpContext) -> String {
        match self {
            BootFile::Ipxe(arch) => arch.boot_file(),
            BootFile::HttpIpxe(arch) => format!(
                "http://{}:{}/{}/{}",
                context.server,
                context.http_port,
                arch.ipxe_name(),
                arch.snp_file()
            ),
            BootFile::Script => "boot.ipxe".to_string(),
        }
    }
}

/// A reply, and what it was for, for the log.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The DHCPOFFER or DHCPACK to send.
    pub reply: Message,
    /// The client's MAC address.
    pub mac: MacAddress,
    /// Option 93, the client's firmware architecture, if it sent one.
    pub client_arch: Option<u16>,
    /// What it was told to boot.
    pub file: BootFile,
}

/// Why a request got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignored {
    /// Not a BOOTREQUEST, or not Ethernet.
    NotEthernetRequest,
    /// No `PXEClient` or `HTTPClient` vendor class: ordinary DHCP traffic.
    NotPxe,
    /// A message type this server doesn't answer on that port.
    NotOurs,
    /// A PXE client `--mac` doesn't list.
    NotAllowed(MacAddress),
    /// Firmware this server has nothing for (BIOS, say).
    UnsupportedArch(MacAddress, Option<u16>),
    /// An architecture whose artefacts aren't in the served directory.
    NotServed(MacAddress, Arch),
}

impl Ignored {
    /// A line for the log, or `None` for traffic that isn't for us at all.
    pub fn describe(&self) -> Option<String> {
        match self {
            Ignored::NotEthernetRequest | Ignored::NotPxe | Ignored::NotOurs => None,
            Ignored::NotAllowed(mac) => Some(format!("{mac}: ignored, not on the --mac list")),
            Ignored::UnsupportedArch(mac, arch) => Some(format!(
                "{mac}: ignored, {} firmware isn't supported (UEFI only)",
                describe_arch(*arch)
            )),
            Ignored::NotServed(mac, arch) => Some(format!(
                "{mac}: ignored, an {arch} machine and no {arch}/ artefacts are served"
            )),
        }
    }
}

/// Option 93's architecture type, as a firmware description (RFC 4578,
/// and the IANA registry for the HTTP Boot values).
pub fn describe_arch(arch: Option<u16>) -> String {
    match arch {
        None => "unknown".to_string(),
        Some(0) => "BIOS".to_string(),
        Some(6) => "IA32 UEFI".to_string(),
        Some(7) | Some(9) => "x86_64 UEFI".to_string(),
        Some(11) => "arm64 UEFI".to_string(),
        Some(16) => "x86_64 UEFI HTTP Boot".to_string(),
        Some(19) => "arm64 UEFI HTTP Boot".to_string(),
        Some(other) => format!("architecture {other}"),
    }
}

/// Decide the reply to `request`, received on `port`.
pub fn answer(
    request: &Message,
    port: ListenPort,
    context: &DhcpContext,
) -> Result<Answer, Ignored> {
    let mac = client_mac(request).ok_or(Ignored::NotEthernetRequest)?;
    let http_client = match class_identifier(request) {
        Some(class) if class.starts_with(b"PXEClient") => false,
        Some(class) if class.starts_with(b"HTTPClient") => true,
        _ => return Err(Ignored::NotPxe),
    };
    let reply_type = reply_type(request, port, context.server).ok_or(Ignored::NotOurs)?;
    if !context.allowed.is_empty() && !context.allowed.contains(&mac) {
        return Err(Ignored::NotAllowed(mac));
    }
    let client_arch = client_arch(request);
    let file = boot_file(mac, client_arch, is_ipxe(request), http_client)?;
    if let BootFile::Ipxe(arch) | BootFile::HttpIpxe(arch) = file
        && !context.architectures.contains(&arch)
    {
        return Err(Ignored::NotServed(mac, arch));
    }
    let reply = build_reply(request, reply_type, port, &file, context);
    Ok(Answer {
        reply,
        mac,
        client_arch,
        file,
    })
}

fn client_mac(request: &Message) -> Option<MacAddress> {
    // `chaddr()` slices by the packet's own hlen, so check it first.
    if request.opcode() != Opcode::BootRequest
        || request.htype() != HType::Eth
        || request.hlen() != 6
    {
        return None;
    }
    let bytes: [u8; 6] = request.chaddr().try_into().ok()?;
    Some(MacAddress(bytes))
}

fn class_identifier(request: &Message) -> Option<&[u8]> {
    match request.opts().get(OptionCode::ClassIdentifier) {
        Some(DhcpOption::ClassIdentifier(class)) => Some(class),
        _ => None,
    }
}

fn reply_type(request: &Message, port: ListenPort, server: Ipv4Addr) -> Option<MessageType> {
    match (port, request.opts().msg_type()?) {
        (ListenPort::Dhcp, MessageType::Discover) => Some(MessageType::Offer),
        (ListenPort::Dhcp, MessageType::Request) if server_identifier(request) == Some(server) => {
            Some(MessageType::Ack)
        }
        (ListenPort::ProxyDhcp, MessageType::Request) => Some(MessageType::Ack),
        _ => None,
    }
}

fn server_identifier(message: &Message) -> Option<Ipv4Addr> {
    match message.opts().get(OptionCode::ServerIdentifier) {
        Some(DhcpOption::ServerIdentifier(address)) => Some(*address),
        _ => None,
    }
}

fn client_arch(request: &Message) -> Option<u16> {
    match request.opts().get(OptionCode::ClientSystemArchitecture) {
        Some(DhcpOption::ClientSystemArchitecture(arch)) => Some(u16::from(*arch)),
        _ => None,
    }
}

/// iPXE sends user class (option 77) `iPXE`, without RFC 3004's length
/// prefix; accept it with one too.
fn is_ipxe(request: &Message) -> bool {
    match request.opts().get(OptionCode::UserClass) {
        Some(DhcpOption::UserClass(class)) => {
            class == b"iPXE" || class.get(1..).is_some_and(|rest| rest == b"iPXE")
        }
        _ => false,
    }
}

fn boot_file(
    mac: MacAddress,
    client_arch: Option<u16>,
    ipxe: bool,
    http_client: bool,
) -> Result<BootFile, Ignored> {
    let arch = match client_arch {
        Some(6 | 7 | 9 | 16) => Arch::X86_64,
        Some(11 | 19) => Arch::Arm64,
        _ if ipxe => return Ok(BootFile::Script),
        other => return Err(Ignored::UnsupportedArch(mac, other)),
    };
    Ok(match client_arch {
        _ if ipxe => BootFile::Script,
        Some(16 | 19) => BootFile::HttpIpxe(arch),
        _ if http_client => BootFile::HttpIpxe(arch),
        _ => BootFile::Ipxe(arch),
    })
}

fn build_reply(
    request: &Message,
    reply_type: MessageType,
    port: ListenPort,
    file: &BootFile,
    context: &DhcpContext,
) -> Message {
    let mut reply = Message::default();
    reply
        .set_opcode(Opcode::BootReply)
        .set_htype(HType::Eth)
        .set_xid(request.xid())
        .set_flags(request.flags())
        .set_chaddr(request.chaddr())
        .set_giaddr(request.giaddr())
        .set_ciaddr(request.ciaddr())
        .set_yiaddr(Ipv4Addr::UNSPECIFIED)
        .set_siaddr(context.server);
    let name = file.name(context);
    // The BOOTP field holds 128 bytes; option 67 alone carries a longer URL.
    if name.len() < 128 {
        reply.set_fname(name.as_bytes());
    }
    let http = matches!(file, BootFile::HttpIpxe(_));
    let class: &[u8] = if http { b"HTTPClient" } else { b"PXEClient" };
    let options = reply.opts_mut();
    options.insert(DhcpOption::MessageType(reply_type));
    options.insert(DhcpOption::ServerIdentifier(context.server));
    options.insert(DhcpOption::ClassIdentifier(class.to_vec()));
    options.insert(DhcpOption::BootfileName(name.into_bytes()));
    if let Some(DhcpOption::ClientMachineIdentifier(uuid)) =
        request.opts().get(OptionCode::ClientMachineIdentifier)
    {
        options.insert(DhcpOption::ClientMachineIdentifier(uuid.clone()));
    }
    if !http {
        options.insert(DhcpOption::VendorExtensions(pxe_vendor_options(
            request, port,
        )));
    }
    reply
}

/// Option 43 for a PXE reply: discovery control, plus the request's boot
/// item echoed on 4011 (firmware doing boot server discovery checks it).
fn pxe_vendor_options(request: &Message, port: ListenPort) -> Vec<u8> {
    let mut options = DISCOVERY_CONTROL.to_vec();
    if port == ListenPort::ProxyDhcp
        && let Some(DhcpOption::VendorExtensions(theirs)) =
            request.opts().get(OptionCode::VendorExtensions)
        && let Some(item) = find_suboption(theirs, BOOT_ITEM)
    {
        options.push(BOOT_ITEM);
        options.push(item.len() as u8);
        options.extend_from_slice(item);
    }
    options.push(255);
    options
}

fn find_suboption(options: &[u8], wanted: u8) -> Option<&[u8]> {
    let mut rest = options;
    loop {
        match rest {
            [] | [255, ..] => return None,
            [0, tail @ ..] => rest = tail,
            [code, len, tail @ ..] => {
                let value = tail.get(..usize::from(*len))?;
                if *code == wanted {
                    return Some(value);
                }
                rest = &tail[value.len()..];
            }
            [_] => return None,
        }
    }
}

/// Where to send the reply to a request that came from `source`.
pub fn destination(request: &Message, port: ListenPort, source: SocketAddrV4) -> SocketAddrV4 {
    match port {
        ListenPort::ProxyDhcp => source,
        ListenPort::Dhcp if !request.giaddr().is_unspecified() => {
            SocketAddrV4::new(request.giaddr(), DHCP_SERVER_PORT)
        }
        ListenPort::Dhcp if !request.ciaddr().is_unspecified() => {
            SocketAddrV4::new(request.ciaddr(), DHCP_CLIENT_PORT)
        }
        // The client has no address yet: broadcast, as the router does.
        ListenPort::Dhcp => SocketAddrV4::new(Ipv4Addr::BROADCAST, DHCP_CLIENT_PORT),
    }
}

/// Parse a datagram, or `None` if it isn't DHCP.
pub fn decode(bytes: &[u8]) -> Option<Message> {
    // dhcproto has `debug_assert!`s and unchecked subtractions on a few
    // option lengths. A crafted packet must not take the server down, so a
    // panic while parsing counts as "not DHCP". Decoding only reads `bytes`
    // and builds a new value, so nothing is left half-changed.
    std::panic::catch_unwind(|| Message::decode(&mut Decoder::new(bytes)).ok())
        .ok()
        .flatten()
}

/// Encode a reply, padded to the BOOTP minimum.
pub fn encode(message: &Message) -> Option<Vec<u8>> {
    let mut bytes = message.to_vec().ok()?;
    if bytes.len() < MIN_MESSAGE_LEN {
        bytes.resize(MIN_MESSAGE_LEN, 0);
    }
    Some(bytes)
}

/// A DHCPDISCOVER that looks like UEFI PXE firmware, for checking that no
/// other ProxyDHCP answers on the LAN before we start.
pub fn probe_discover(xid: u32, mac: MacAddress) -> Message {
    let mut message = Message::default();
    message
        .set_opcode(Opcode::BootRequest)
        .set_htype(HType::Eth)
        .set_xid(xid)
        // We aren't on the address an answer would be sent to.
        .set_flags(Flags::default().set_broadcast())
        .set_chaddr(&mac.0);
    let options = message.opts_mut();
    options.insert(DhcpOption::MessageType(MessageType::Discover));
    options.insert(DhcpOption::ClassIdentifier(
        b"PXEClient:Arch:00007:UNDI:003016".to_vec(),
    ));
    options.insert(DhcpOption::ClientSystemArchitecture(Architecture::from(7)));
    options.insert(DhcpOption::ParameterRequestList(vec![
        OptionCode::VendorExtensions,
        OptionCode::ClassIdentifier,
        OptionCode::TFTPServerName,
        OptionCode::BootfileName,
    ]));
    message
}

/// If `reply` answers our probe `xid` as a PXE boot server would, the
/// server's address. A router's plain offer doesn't count: only one that
/// says `PXEClient` or names a boot file.
pub fn competing_server(reply: &Message, xid: u32, source: Ipv4Addr) -> Option<Ipv4Addr> {
    if reply.opcode() != Opcode::BootReply
        || reply.xid() != xid
        || reply.opts().msg_type() != Some(MessageType::Offer)
    {
        return None;
    }
    let pxe = class_identifier(reply).is_some_and(|c| c.starts_with(b"PXEClient"));
    let boot_file = reply.opts().get(OptionCode::BootfileName).is_some()
        || reply.fname().is_some_and(|f| f.iter().any(|b| *b != 0));
    if !pxe && !boot_file {
        return None;
    }
    server_identifier(reply)
        .or_else(|| Some(reply.siaddr()).filter(|a| !a.is_unspecified()))
        .or(Some(source))
}

/// Answer PXE requests arriving on `socket` until it fails. Each answer
/// and each refusal worth knowing about goes to `log`.
pub async fn serve(
    socket: Arc<UdpSocket>,
    port: ListenPort,
    context: Arc<DhcpContext>,
    log: impl Fn(String),
) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 1500];
    loop {
        let (len, source) = socket.recv_from(&mut buffer).await?;
        let std::net::SocketAddr::V4(source) = source else {
            continue;
        };
        let Some(request) = decode(&buffer[..len]) else {
            continue;
        };
        match answer(&request, port, &context) {
            Ok(answer) => {
                let Some(bytes) = encode(&answer.reply) else {
                    continue;
                };
                let to = destination(&request, port, source);
                if let Err(e) = socket.send_to(&bytes, to).await {
                    log(format!(
                        "dhcp: {}: couldn't send the reply to {to}: {e}",
                        answer.mac
                    ));
                    continue;
                }
                log(describe_answer(&answer, port, &context));
            }
            Err(ignored) => {
                if let Some(line) = ignored.describe() {
                    log(format!("dhcp: {line}"));
                }
            }
        }
    }
}

fn describe_answer(answer: &Answer, port: ListenPort, context: &DhcpContext) -> String {
    let kind = match (port, answer.reply.opts().msg_type()) {
        (_, Some(MessageType::Offer)) => "offer",
        (ListenPort::ProxyDhcp, _) => "ack on 4011",
        _ => "ack",
    };
    let who = if answer.file == BootFile::Script {
        "iPXE".to_string()
    } else {
        describe_arch(answer.client_arch)
    };
    format!(
        "dhcp: {} ({who}): {kind}, boot {}",
        answer.mac,
        answer.file.name(context)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SERVER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn context() -> DhcpContext {
        DhcpContext {
            server: SERVER,
            http_port: 8080,
            architectures: [Arch::X86_64, Arch::Arm64].into(),
            allowed: Vec::new(),
        }
    }

    fn request(kind: MessageType, class: &[u8], arch: Option<u16>) -> Message {
        let mut message = Message::default();
        message
            .set_opcode(Opcode::BootRequest)
            .set_htype(HType::Eth)
            .set_xid(0x1234_5678)
            .set_chaddr(&MAC);
        let options = message.opts_mut();
        options.insert(DhcpOption::MessageType(kind));
        options.insert(DhcpOption::ClassIdentifier(class.to_vec()));
        if let Some(arch) = arch {
            options.insert(DhcpOption::ClientSystemArchitecture(Architecture::from(
                arch,
            )));
        }
        message
    }

    fn pxe_discover(arch: u16) -> Message {
        request(
            MessageType::Discover,
            b"PXEClient:Arch:00007:UNDI:003016",
            Some(arch),
        )
    }

    fn option(message: &Message, code: OptionCode) -> Option<&DhcpOption> {
        message.opts().get(code)
    }

    fn bootfile(message: &Message) -> Vec<u8> {
        match option(message, OptionCode::BootfileName) {
            Some(DhcpOption::BootfileName(name)) => name.clone(),
            other => panic!("no option 67: {other:?}"),
        }
    }

    #[test]
    fn a_uefi_pxe_discover_gets_an_offer_for_ipxe_without_an_address() {
        let answer = answer(&pxe_discover(7), ListenPort::Dhcp, &context()).unwrap();
        let reply = &answer.reply;
        assert_eq!(reply.opcode(), Opcode::BootReply);
        assert_eq!(reply.opts().msg_type(), Some(MessageType::Offer));
        assert_eq!(reply.xid(), 0x1234_5678);
        assert_eq!(reply.chaddr(), MAC);
        assert_eq!(reply.yiaddr(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(reply.siaddr(), SERVER);
        assert_eq!(
            option(reply, OptionCode::ServerIdentifier),
            Some(&DhcpOption::ServerIdentifier(SERVER))
        );
        assert_eq!(
            option(reply, OptionCode::ClassIdentifier),
            Some(&DhcpOption::ClassIdentifier(b"PXEClient".to_vec()))
        );
        assert_eq!(
            option(reply, OptionCode::VendorExtensions),
            Some(&DhcpOption::VendorExtensions(vec![6, 1, 8, 255]))
        );
        assert_eq!(bootfile(reply), b"ipxe-x86_64.efi");
        assert_eq!(reply.fname(), Some(&b"ipxe-x86_64.efi"[..]));
        assert_eq!(answer.file, BootFile::Ipxe(Arch::X86_64));
    }

    #[test]
    fn each_firmware_architecture_gets_its_own_boot_file() {
        let cases: [(u16, &[u8]); 6] = [
            (6, b"ipxe-x86_64.efi"),
            (7, b"ipxe-x86_64.efi"),
            (9, b"ipxe-x86_64.efi"),
            (11, b"ipxe-arm64.efi"),
            (16, b"http://192.168.1.20:8080/x86_64/ipxe-snp-x86_64.efi"),
            (19, b"http://192.168.1.20:8080/arm64/ipxe-snp-arm64.efi"),
        ];
        for (arch, file) in cases {
            let answer = answer(&pxe_discover(arch), ListenPort::Dhcp, &context()).unwrap();
            assert_eq!(bootfile(&answer.reply), file, "architecture {arch}");
            assert_eq!(answer.reply.fname(), Some(file), "architecture {arch}");
        }
    }

    #[test]
    fn http_boot_clients_are_answered_as_http_clients_without_pxe_options() {
        let discover = request(
            MessageType::Discover,
            b"HTTPClient:Arch:00016:UNDI:003001",
            Some(16),
        );
        let reply = answer(&discover, ListenPort::Dhcp, &context())
            .unwrap()
            .reply;
        assert_eq!(
            option(&reply, OptionCode::ClassIdentifier),
            Some(&DhcpOption::ClassIdentifier(b"HTTPClient".to_vec()))
        );
        assert_eq!(option(&reply, OptionCode::VendorExtensions), None);
    }

    #[test]
    fn ipxe_asking_for_its_script_gets_boot_ipxe() {
        for user_class in [&b"iPXE"[..], b"\x04iPXE"] {
            let mut discover = pxe_discover(7);
            discover
                .opts_mut()
                .insert(DhcpOption::UserClass(user_class.to_vec()));
            let answer = answer(&discover, ListenPort::Dhcp, &context()).unwrap();
            assert_eq!(bootfile(&answer.reply), b"boot.ipxe");
            assert_eq!(answer.file, BootFile::Script);
        }
        let mut no_arch = request(MessageType::Discover, b"PXEClient", None);
        no_arch
            .opts_mut()
            .insert(DhcpOption::UserClass(b"iPXE".to_vec()));
        assert_eq!(
            answer(&no_arch, ListenPort::Dhcp, &context()).unwrap().file,
            BootFile::Script
        );
    }

    #[test]
    fn the_client_uuid_is_echoed() {
        let mut discover = pxe_discover(7);
        let uuid = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        discover
            .opts_mut()
            .insert(DhcpOption::ClientMachineIdentifier(uuid.to_vec()));
        let reply = answer(&discover, ListenPort::Dhcp, &context())
            .unwrap()
            .reply;
        assert_eq!(
            option(&reply, OptionCode::ClientMachineIdentifier),
            Some(&DhcpOption::ClientMachineIdentifier(uuid.to_vec()))
        );
        let without = answer(&pxe_discover(7), ListenPort::Dhcp, &context())
            .unwrap()
            .reply;
        assert_eq!(option(&without, OptionCode::ClientMachineIdentifier), None);
    }

    #[test]
    fn a_request_on_4011_gets_an_ack_echoing_its_boot_item() {
        let mut proxy_request = pxe_discover(7);
        proxy_request
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Request));
        proxy_request
            .opts_mut()
            .insert(DhcpOption::VendorExtensions(vec![
                71, 4, 0x80, 0x00, 0x00, 0x00, 255,
            ]));
        proxy_request.set_ciaddr(Ipv4Addr::new(192, 168, 1, 51));
        let reply = answer(&proxy_request, ListenPort::ProxyDhcp, &context())
            .unwrap()
            .reply;
        assert_eq!(reply.opts().msg_type(), Some(MessageType::Ack));
        assert_eq!(reply.yiaddr(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(reply.ciaddr(), Ipv4Addr::new(192, 168, 1, 51));
        assert_eq!(
            option(&reply, OptionCode::VendorExtensions),
            Some(&DhcpOption::VendorExtensions(vec![
                6, 1, 8, 71, 4, 0x80, 0, 0, 0, 255
            ]))
        );
    }

    #[test]
    fn a_request_on_67_is_answered_only_when_it_names_this_server() {
        let mut to_us = pxe_discover(7);
        to_us
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Request));
        to_us
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier(SERVER));
        let reply = answer(&to_us, ListenPort::Dhcp, &context()).unwrap().reply;
        assert_eq!(reply.opts().msg_type(), Some(MessageType::Ack));

        let mut to_router = to_us.clone();
        to_router
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(
            answer(&to_router, ListenPort::Dhcp, &context()).unwrap_err(),
            Ignored::NotOurs
        );
    }

    #[test]
    fn other_message_types_are_not_answered() {
        for kind in [
            MessageType::Release,
            MessageType::Inform,
            MessageType::Decline,
            MessageType::Offer,
        ] {
            let message = request(kind, b"PXEClient", Some(7));
            assert_eq!(
                answer(&message, ListenPort::Dhcp, &context()).unwrap_err(),
                Ignored::NotOurs
            );
        }
        let discover_on_4011 = pxe_discover(7);
        assert_eq!(
            answer(&discover_on_4011, ListenPort::ProxyDhcp, &context()).unwrap_err(),
            Ignored::NotOurs
        );
    }

    #[test]
    fn ordinary_dhcp_clients_are_ignored_silently() {
        let laptop = request(MessageType::Discover, b"MSFT 5.0", None);
        let error = answer(&laptop, ListenPort::Dhcp, &context()).unwrap_err();
        assert_eq!(error, Ignored::NotPxe);
        assert_eq!(error.describe(), None);
        let mut no_class = pxe_discover(7);
        no_class.opts_mut().remove(OptionCode::ClassIdentifier);
        assert_eq!(
            answer(&no_class, ListenPort::Dhcp, &context()).unwrap_err(),
            Ignored::NotPxe
        );
    }

    #[test]
    fn bios_firmware_and_unknown_architectures_are_refused_with_a_reason() {
        let mac = MacAddress(MAC);
        for arch in [Some(0), Some(10), None] {
            let message = request(MessageType::Discover, b"PXEClient", arch);
            let error = answer(&message, ListenPort::Dhcp, &context()).unwrap_err();
            assert_eq!(error, Ignored::UnsupportedArch(mac, arch));
            assert!(error.describe().is_some());
        }
    }

    #[test]
    fn an_architecture_without_artefacts_is_refused() {
        let mut only_x86 = context();
        only_x86.architectures = [Arch::X86_64].into();
        assert_eq!(
            answer(&pxe_discover(11), ListenPort::Dhcp, &only_x86).unwrap_err(),
            Ignored::NotServed(MacAddress(MAC), Arch::Arm64)
        );
    }

    #[test]
    fn the_mac_list_answers_only_the_machines_on_it() {
        let mut allow = context();
        allow.allowed = vec![MacAddress(MAC)];
        assert!(answer(&pxe_discover(7), ListenPort::Dhcp, &allow).is_ok());
        allow.allowed = vec![MacAddress([0x52, 0x54, 0, 0, 0, 1])];
        assert_eq!(
            answer(&pxe_discover(7), ListenPort::Dhcp, &allow).unwrap_err(),
            Ignored::NotAllowed(MacAddress(MAC))
        );
    }

    #[test]
    fn non_ethernet_and_oversized_hardware_addresses_are_ignored() {
        let mut long = pxe_discover(7);
        long.set_chaddr(&[1; 16]);
        assert_eq!(
            answer(&long, ListenPort::Dhcp, &context()).unwrap_err(),
            Ignored::NotEthernetRequest
        );
        let mut reply = pxe_discover(7);
        reply.set_opcode(Opcode::BootReply);
        assert_eq!(
            answer(&reply, ListenPort::Dhcp, &context()).unwrap_err(),
            Ignored::NotEthernetRequest
        );
    }

    #[test]
    fn a_reply_survives_encoding_and_is_at_least_300_bytes() {
        let reply = answer(&pxe_discover(7), ListenPort::Dhcp, &context())
            .unwrap()
            .reply;
        let bytes = encode(&reply).unwrap();
        assert!(bytes.len() >= 300);
        // The BOOTP header: op 2, htype 1, hlen 6; yiaddr at 16..20, siaddr
        // at 20..24, file at 108..236, the magic cookie at 236..240.
        assert_eq!(&bytes[..3], &[2, 1, 6]);
        assert_eq!(&bytes[16..20], &[0, 0, 0, 0]);
        assert_eq!(&bytes[20..24], &SERVER.octets());
        assert_eq!(&bytes[108..123], b"ipxe-x86_64.efi");
        assert_eq!(&bytes[236..240], &[99, 130, 83, 99]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.opts(), reply.opts());
        assert_eq!(decoded.xid(), reply.xid());
        assert_eq!(decoded.chaddr(), reply.chaddr());
    }

    #[test]
    fn replies_go_by_broadcast_unless_the_client_has_an_address() {
        let source = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 68);
        let discover = pxe_discover(7);
        assert_eq!(
            destination(&discover, ListenPort::Dhcp, source),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 68)
        );
        let mut renewing = discover.clone();
        renewing.set_ciaddr(Ipv4Addr::new(192, 168, 1, 51));
        assert_eq!(
            destination(&renewing, ListenPort::Dhcp, source),
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 51), 68)
        );
        let mut relayed = discover.clone();
        relayed.set_giaddr(Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(
            destination(&relayed, ListenPort::Dhcp, source),
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 67)
        );
        let peer = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 51), 4011);
        assert_eq!(destination(&renewing, ListenPort::ProxyDhcp, peer), peer);
    }

    #[tokio::test]
    async fn a_proxy_request_over_loopback_gets_an_ack_and_ordinary_dhcp_gets_nothing() {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server = socket.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(serve(
            socket,
            ListenPort::ProxyDhcp,
            Arc::new(context()),
            move |line| {
                let _ = tx.send(line);
            },
        ));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let laptop = request(MessageType::Request, b"MSFT 5.0", None);
        client
            .send_to(&encode(&laptop).unwrap(), server)
            .await
            .unwrap();
        let mut ipxe = request(MessageType::Request, b"PXEClient:Arch:00007", Some(7));
        ipxe.opts_mut()
            .insert(DhcpOption::UserClass(b"iPXE".to_vec()));
        client
            .send_to(&encode(&ipxe).unwrap(), server)
            .await
            .unwrap();

        let mut buffer = vec![0u8; 1500];
        let (len, from) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(from, server);
        let reply = decode(&buffer[..len]).unwrap();
        assert_eq!(reply.opts().msg_type(), Some(MessageType::Ack));
        assert_eq!(bootfile(&reply), b"boot.ipxe");
        let line = rx.recv().await.unwrap();
        assert_eq!(
            line,
            "dhcp: 52:54:00:12:34:56 (iPXE): ack on 4011, boot boot.ipxe"
        );
        assert!(rx.try_recv().is_err(), "the laptop's request is not logged");
        task.abort();
    }

    #[test]
    fn garbage_is_not_dhcp() {
        assert!(decode(b"").is_none());
        assert!(decode(&[0u8; 100]).is_none());
    }

    #[test]
    fn a_probe_looks_like_uefi_pxe_firmware() {
        let probe = probe_discover(42, MacAddress(MAC));
        assert!(probe.flags().broadcast());
        let pxe = answer(&probe, ListenPort::Dhcp, &context()).unwrap();
        assert_eq!(pxe.file, BootFile::Ipxe(Arch::X86_64));
    }

    #[test]
    fn another_proxydhcp_answering_the_probe_is_noticed_but_a_router_is_not() {
        let probe = probe_discover(42, MacAddress(MAC));
        let other = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 30), 67);
        let pxe_offer = answer(&probe, ListenPort::Dhcp, &context()).unwrap().reply;
        assert_eq!(competing_server(&pxe_offer, 42, *other.ip()), Some(SERVER));
        assert_eq!(competing_server(&pxe_offer, 43, *other.ip()), None);

        let mut router_offer = Message::default();
        router_offer
            .set_opcode(Opcode::BootReply)
            .set_xid(42)
            .set_yiaddr(Ipv4Addr::new(192, 168, 1, 77))
            .set_siaddr(Ipv4Addr::new(192, 168, 1, 1));
        router_offer
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Offer));
        assert_eq!(competing_server(&router_offer, 42, *other.ip()), None);

        router_offer.set_fname(b"pxelinux.0");
        assert_eq!(
            competing_server(&router_offer, 42, *other.ip()),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
    }

    fn arbitrary_request() -> impl Strategy<Value = (Message, ListenPort)> {
        let kinds = prop::sample::select(vec![
            MessageType::Discover,
            MessageType::Request,
            MessageType::Inform,
            MessageType::Release,
            MessageType::Offer,
        ]);
        let class = prop_oneof![
            Just(b"PXEClient:Arch:00007:UNDI:003016".to_vec()),
            Just(b"HTTPClient:Arch:00016".to_vec()),
            prop::collection::vec(any::<u8>(), 0..40),
        ];
        (
            kinds,
            prop::option::of(class),
            prop::option::of(any::<u16>()),
            any::<bool>(),
            prop::collection::vec(any::<u8>(), 0..17),
            any::<[u8; 4]>(),
            any::<bool>(),
        )
            .prop_map(|(kind, class, arch, ipxe, chaddr, ciaddr, proxy)| {
                let mut message = Message::default();
                message
                    .set_opcode(Opcode::BootRequest)
                    .set_htype(HType::Eth)
                    .set_chaddr(&chaddr)
                    .set_ciaddr(Ipv4Addr::from(ciaddr))
                    .set_yiaddr(Ipv4Addr::new(10, 9, 8, 7));
                let options = message.opts_mut();
                options.insert(DhcpOption::MessageType(kind));
                if let Some(class) = class {
                    options.insert(DhcpOption::ClassIdentifier(class));
                }
                if let Some(arch) = arch {
                    options.insert(DhcpOption::ClientSystemArchitecture(Architecture::from(
                        arch,
                    )));
                }
                if ipxe {
                    options.insert(DhcpOption::UserClass(b"iPXE".to_vec()));
                }
                let port = if proxy {
                    ListenPort::ProxyDhcp
                } else {
                    ListenPort::Dhcp
                };
                (message, port)
            })
    }

    proptest! {
        #[test]
        fn no_reply_ever_assigns_an_address((message, port) in arbitrary_request()) {
            if let Ok(answer) = answer(&message, port, &context()) {
                prop_assert_eq!(answer.reply.yiaddr(), Ipv4Addr::UNSPECIFIED);
                let bytes = encode(&answer.reply).unwrap();
                prop_assert_eq!(&bytes[16..20], &[0u8, 0, 0, 0][..]);
            }
        }

        #[test]
        fn clients_without_a_pxe_or_http_boot_class_are_never_answered(
            (message, port) in arbitrary_request()
        ) {
            let pxe = class_identifier(&message)
                .is_some_and(|c| c.starts_with(b"PXEClient") || c.starts_with(b"HTTPClient"));
            if !pxe {
                prop_assert!(answer(&message, port, &context()).is_err());
            }
        }

        #[test]
        fn any_datagram_parses_or_is_dropped_without_panicking(
            bytes in prop::collection::vec(any::<u8>(), 0..600)
        ) {
            let _ = decode(&bytes);
        }
    }
}

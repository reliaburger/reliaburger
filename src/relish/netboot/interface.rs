//! Which interface to answer on, and sockets bound to it.
//!
//! PXE firmware broadcasts, so the DHCP sockets listen on 0.0.0.0. On a
//! machine with several interfaces (a laptop with Wi-Fi, a VPN and a
//! container bridge), each socket is also tied to the chosen interface,
//! `SO_BINDTODEVICE`-style, so replies broadcast on the LAN rather than on
//! whatever the routing table prefers.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::num::NonZeroU32;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use super::{InterfaceChoice, NetbootError};

/// A local IPv4 interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// Its name, such as `eth0` or `en0`.
    pub name: String,
    /// Its IPv4 address.
    pub address: Ipv4Addr,
    /// True for the loopback interface.
    pub loopback: bool,
}

/// Every interface that's up and has an IPv4 address.
pub fn list() -> Result<Vec<Interface>, NetbootError> {
    let addresses = nix::ifaddrs::getifaddrs()
        .map_err(|e| NetbootError::Interface(format!("can't list network interfaces: {e}")))?;
    Ok(addresses
        .filter(|a| a.flags.contains(nix::net::if_::InterfaceFlags::IFF_UP))
        .filter_map(|a| {
            let address = a.address?.as_sockaddr_in()?.ip();
            Some(Interface {
                loopback: a
                    .flags
                    .contains(nix::net::if_::InterfaceFlags::IFF_LOOPBACK),
                name: a.interface_name,
                address,
            })
        })
        .collect())
}

/// The address the default route leaves from, if there is a default route.
/// Connecting a UDP socket sends nothing; it only picks a route.
pub fn default_route_address() -> Option<Ipv4Addr> {
    let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // TEST-NET-1: never on a real network, but routed by the default route.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_unspecified() => Some(address),
        _ => None,
    }
}

/// Pick the interface `choice` names from `candidates`.
pub fn pick(
    candidates: &[Interface],
    choice: &InterfaceChoice,
    default_route: Option<Ipv4Addr>,
) -> Result<Interface, NetbootError> {
    let found = match choice {
        // An interface can hold a self-assigned address beside a real one:
        // prefer the real one.
        InterfaceChoice::Named(name) => {
            let named = || candidates.iter().filter(|i| &i.name == name);
            named()
                .find(|i| !i.address.is_link_local())
                .or_else(|| named().next())
        }
        InterfaceChoice::Address(address) => candidates.iter().find(|i| i.address == *address),
        InterfaceChoice::DefaultRoute => default_route.and_then(|address| {
            candidates
                .iter()
                .find(|i| i.address == address && !i.loopback)
        }),
    };
    if let Some(found) = found
        && found.address.is_link_local()
    {
        return Err(self_assigned(found));
    }
    found.cloned().ok_or_else(|| {
        let available: Vec<String> = candidates
            .iter()
            .filter(|i| !i.loopback)
            .map(|i| format!("{} ({})", i.name, i.address))
            .collect();
        let available = if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        };
        let what = match choice {
            InterfaceChoice::Named(name) => format!("no interface {name} with an IPv4 address"),
            InterfaceChoice::Address(address) => format!("no interface has address {address}"),
            InterfaceChoice::DefaultRoute => {
                "no default route to pick an interface from; name one with --interface".to_string()
            }
        };
        NetbootError::Interface(format!("{what} (IPv4 interfaces: {available})"))
    })
}

/// The refusal for an interface whose only address is self-assigned
/// (169.254.0.0/16). macOS and Windows give an interface one when no DHCP
/// server answered it. Machines that PXE-boot get their addresses from the
/// LAN's DHCP server, so they'd be told to fetch iPXE from an address they
/// can't reach.
fn self_assigned(interface: &Interface) -> NetbootError {
    NetbootError::Interface(format!(
        "{} has only {}, a self-assigned address, so no DHCP server answered it and the machines you boot couldn't reach this one. \
         Check the cable and that the LAN's DHCP server (usually your router) is up, then reconnect the interface; \
         or give it a fixed address on the LAN and pass that with --address",
        interface.name, interface.address
    ))
}

/// Find the interface to serve on.
pub fn find(choice: &InterfaceChoice) -> Result<Interface, NetbootError> {
    pick(&list()?, choice, default_route_address())
}

/// A UDP socket on `address`, tied to `interface` when given, with
/// `SO_BROADCAST`. It shares its port with nothing, so a second netboot
/// server fails with [`NetbootError::PortInUse`]. `what` names it in
/// errors. Binding a privileged port without root fails with
/// [`NetbootError::NeedsRoot`].
pub fn udp_socket(
    what: &'static str,
    address: SocketAddrV4,
    interface: Option<&Interface>,
) -> Result<UdpSocket, NetbootError> {
    bind_udp(what, address, interface, false)
}

/// The socket the start-up probe listens for offers on: 0.0.0.0 on `port`
/// (the DHCP client port, 68, except in tests), tied to `interface`.
///
/// The machine's own DHCP client may hold port 68 (configd's
/// IPConfiguration on macOS, dhclient on Linux). `SO_REUSEPORT` lets the
/// probe share the port with a socket that set it too (and on Linux,
/// `SO_REUSEADDR` with one that set that), and since DHCP servers
/// broadcast their answers to a probe, both sockets get a copy. A holder
/// that set neither keeps the port to itself, and the bind fails with
/// [`NetbootError::PortInUse`]. The servers' own sockets set neither: two
/// netboot servers sharing port 67 would race for every machine instead
/// of the second failing to start.
pub fn probe_socket(port: u16, interface: Option<&Interface>) -> Result<UdpSocket, NetbootError> {
    let address = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    bind_udp("the DHCP client port", address, interface, true)
}

fn bind_udp(
    what: &'static str,
    address: SocketAddrV4,
    interface: Option<&Interface>,
    share_port: bool,
) -> Result<UdpSocket, NetbootError> {
    let fail = |e: std::io::Error| bind_error(what, address.port(), e);
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(fail)?;
    // Linux lets two UDP sockets that both set SO_REUSEADDR share a port,
    // so a server socket sets neither option and stays exclusive on Linux
    // as on macOS. UDP has no TIME_WAIT, so a restart doesn't need them.
    // The probe sets both: SO_REUSEPORT to share with configd on macOS, and
    // SO_REUSEADDR to share with a Linux DHCP client that set only that.
    if share_port {
        socket.set_reuse_address(true).map_err(fail)?;
        socket.set_reuse_port(true).map_err(fail)?;
    }
    socket.set_broadcast(true).map_err(fail)?;
    if let Some(interface) = interface {
        let index = nix::net::if_::if_nametoindex(interface.name.as_str())
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| NetbootError::Interface(format!("{} has no index", interface.name)))?;
        // Linux before 5.7 wants CAP_NET_RAW for this, which a binary given
        // only cap_net_bind_service lacks. Without it, broadcasts follow
        // the routing table: fine on a machine with one LAN interface.
        if let Err(error) = socket.bind_device_by_index_v4(Some(index)) {
            eprintln!(
                "relish netboot: warning: can't tie {what} to {} ({error}); replies follow the routing table",
                interface.name
            );
        }
    }
    socket.set_nonblocking(true).map_err(fail)?;
    socket.bind(&address.into()).map_err(fail)?;
    UdpSocket::from_std(socket.into()).map_err(fail)
}

/// Turn a bind failure into the error a person can act on.
pub fn bind_error(what: &'static str, port: u16, error: std::io::Error) -> NetbootError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => NetbootError::NeedsRoot,
        std::io::ErrorKind::AddrInUse => NetbootError::PortInUse {
            what,
            port,
            hint: match port {
                67 => {
                    ": another DHCP server runs here (dnsmasq, or macOS Internet Sharing's bootpd?); if it's the lab's own, pass --mode-dhcp-proxy"
                }
                68 => {
                    ": this machine's own DHCP client holds it (configd on macOS, dhclient on Linux)"
                }
                69 => ": another TFTP server runs here",
                _ => "",
            },
        },
        _ => NetbootError::io(format!("{what} (port {port})"), error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interfaces() -> Vec<Interface> {
        vec![
            Interface {
                name: "lo".into(),
                address: Ipv4Addr::LOCALHOST,
                loopback: true,
            },
            Interface {
                name: "eth0".into(),
                address: Ipv4Addr::new(192, 168, 1, 20),
                loopback: false,
            },
            Interface {
                name: "docker0".into(),
                address: Ipv4Addr::new(172, 17, 0, 1),
                loopback: false,
            },
        ]
    }

    #[test]
    fn an_interface_is_found_by_name_by_address_or_by_the_default_route() {
        let all = interfaces();
        let eth0 = &all[1];
        assert_eq!(
            &pick(&all, &InterfaceChoice::Named("eth0".into()), None).unwrap(),
            eth0
        );
        assert_eq!(
            &pick(
                &all,
                &InterfaceChoice::Address(Ipv4Addr::new(192, 168, 1, 20)),
                None
            )
            .unwrap(),
            eth0
        );
        assert_eq!(
            &pick(&all, &InterfaceChoice::DefaultRoute, Some(eth0.address)).unwrap(),
            eth0
        );
    }

    #[test]
    fn a_missing_interface_lists_the_ones_there_are() {
        let error = pick(&interfaces(), &InterfaceChoice::Named("wlan0".into()), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no interface wlan0"), "{error}");
        assert!(
            error.contains("eth0 (192.168.1.20), docker0 (172.17.0.1)"),
            "{error}"
        );
        assert!(!error.contains("127.0.0.1"), "{error}");
        let error = pick(&interfaces(), &InterfaceChoice::DefaultRoute, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--interface"), "{error}");
    }

    fn en7(address: Ipv4Addr) -> Interface {
        Interface {
            name: "en7".into(),
            address,
            loopback: false,
        }
    }

    #[test]
    fn a_self_assigned_address_is_refused_with_what_to_do() {
        let self_assigned = Ipv4Addr::new(169, 254, 12, 34);
        let mut all = interfaces();
        all.push(en7(self_assigned));
        for (choice, default_route) in [
            (InterfaceChoice::Named("en7".into()), None),
            (InterfaceChoice::Address(self_assigned), None),
            (InterfaceChoice::DefaultRoute, Some(self_assigned)),
        ] {
            let error = pick(&all, &choice, default_route).unwrap_err().to_string();
            assert!(error.contains("en7 has only 169.254.12.34"), "{error}");
            assert!(error.contains("self-assigned"), "{error}");
            assert!(error.contains("DHCP server"), "{error}");
            assert!(error.contains("cable"), "{error}");
            assert!(error.contains("--address"), "{error}");
        }
    }

    #[test]
    fn an_interface_with_a_lan_address_as_well_serves_from_that_one() {
        let mut all = interfaces();
        all.push(en7(Ipv4Addr::new(169, 254, 12, 34)));
        all.push(en7(Ipv4Addr::new(10, 77, 0, 2)));
        let picked = pick(&all, &InterfaceChoice::Named("en7".into()), None).unwrap();
        assert_eq!(picked.address, Ipv4Addr::new(10, 77, 0, 2));
    }

    #[test]
    fn bind_failures_say_what_to_do() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(
            bind_error("DHCP", 67, denied),
            NetbootError::NeedsRoot
        ));
        let in_use = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let message = bind_error("DHCP", 67, in_use).to_string();
        assert!(
            message.contains("DHCP (port 67) is in use already"),
            "{message}"
        );
        assert!(message.contains("bootpd"), "{message}");
        let in_use = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let message = bind_error("the DHCP client port", 68, in_use).to_string();
        assert!(message.contains("DHCP client"), "{message}");
        assert!(message.contains("configd"), "{message}");
    }

    /// A socket bound to 0.0.0.0 on an ephemeral port, as the machine's own
    /// DHCP client might hold port 68, with the sharing options given.
    fn hold_a_port(reuse_address: bool, reuse_port: bool) -> (Socket, u16) {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_reuse_address(reuse_address).unwrap();
        socket.set_reuse_port(reuse_port).unwrap();
        let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        socket.bind(&any.into()).unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        (socket, port)
    }

    // The sharing rules differ between the kernels, and CI runs these on
    // Linux while the lab runs relish on macOS:
    // - Linux lets two UDP sockets bind one address and port when both set
    //   SO_REUSEADDR, or when both set SO_REUSEPORT (same user).
    // - BSD and macOS let them only when both set SO_REUSEPORT; their
    //   SO_REUSEADDR shares only multicast addresses.
    // A socket that sets neither is exclusive on both, whatever the other
    // socket set. So the servers set neither, and the probe sets both.

    #[tokio::test]
    async fn the_probe_shares_a_port_another_socket_holds_with_so_reuseport() {
        let (_holder, port) = hold_a_port(true, true);
        let probe = probe_socket(port, None).unwrap();
        assert_eq!(probe.local_addr().unwrap().port(), port);
        assert!(probe.broadcast().unwrap());
    }

    #[tokio::test]
    async fn the_probe_reports_a_port_another_socket_holds_alone() {
        let (_holder, port) = hold_a_port(false, false);
        let error = probe_socket(port, None).unwrap_err();
        assert!(
            matches!(error, NetbootError::PortInUse { port: p, .. } if p == port),
            "{error}"
        );
    }

    #[tokio::test]
    async fn the_servers_sockets_set_neither_sharing_option() {
        let socket = udp_socket("test", SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap();
        let socket = socket2::SockRef::from(&socket);
        assert!(!socket.reuse_address().unwrap());
        assert!(!socket.reuse_port().unwrap());
    }

    #[tokio::test]
    async fn the_servers_sockets_never_share_their_port() {
        // Two relish netboots sharing port 67 would race for every machine
        // instead of the second failing to start.
        let first = udp_socket("DHCP", SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap();
        let port = first.local_addr().unwrap().port();
        let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        assert!(matches!(
            udp_socket("DHCP", any, None),
            Err(NetbootError::PortInUse { .. })
        ));
        // Nor with a socket that offers to share, such as the probe's.
        for (reuse_address, reuse_port) in [(true, false), (false, true), (true, true)] {
            let (_holder, port) = hold_a_port(reuse_address, reuse_port);
            let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
            assert!(
                matches!(
                    udp_socket("DHCP", any, None),
                    Err(NetbootError::PortInUse { .. })
                ),
                "SO_REUSEADDR {reuse_address}, SO_REUSEPORT {reuse_port}"
            );
        }
    }

    #[tokio::test]
    async fn a_broadcast_socket_binds_an_ephemeral_loopback_port() {
        let socket = udp_socket("test", SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap();
        assert!(socket.broadcast().unwrap());
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }

    #[test]
    fn this_machine_lists_its_loopback_interface() {
        assert!(list().unwrap().iter().any(|i| i.loopback));
    }
}

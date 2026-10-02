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
        InterfaceChoice::Named(name) => candidates.iter().find(|i| &i.name == name),
        InterfaceChoice::Address(address) => candidates.iter().find(|i| i.address == *address),
        InterfaceChoice::DefaultRoute => default_route.and_then(|address| {
            candidates
                .iter()
                .find(|i| i.address == address && !i.loopback)
        }),
    };
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

/// Find the interface to serve on.
pub fn find(choice: &InterfaceChoice) -> Result<Interface, NetbootError> {
    pick(&list()?, choice, default_route_address())
}

/// A UDP socket on `address`, tied to `interface` when given, with
/// `SO_BROADCAST` and `SO_REUSEADDR`. `what` names it in errors. Binding
/// a privileged port without root fails with [`NetbootError::NeedsRoot`].
pub fn udp_socket(
    what: &'static str,
    address: SocketAddrV4,
    interface: Option<&Interface>,
) -> Result<UdpSocket, NetbootError> {
    let fail = |e: std::io::Error| bind_error(what, address.port(), e);
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(fail)?;
    socket.set_reuse_address(true).map_err(fail)?;
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
                    ": another DHCP server runs here (dnsmasq, or macOS Internet Sharing's bootpd?)"
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

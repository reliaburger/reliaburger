//! The address an appliance advertises when its seed doesn't name one (G3).
//!
//! A node behind a home router has one wired interface with a DHCP
//! reservation; the address that interface carries is the one its peers
//! should dial. "The interface of the default route" picks it without
//! guessing between docker bridges, loopback or link-local addresses.

use std::net::{IpAddr, Ipv4Addr};

/// The interface of the IPv4 default route, from `/proc/net/route`'s text:
/// the first row whose destination and mask are both zero. With several
/// default routes the kernel uses the lowest metric, so that one wins.
pub fn default_route_interface(proc_net_route: &str) -> Option<String> {
    proc_net_route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (iface, destination, metric, mask) = (
                fields.first()?,
                fields.get(1)?,
                fields.get(6)?,
                fields.get(7)?,
            );
            (*destination == "00000000" && *mask == "00000000")
                .then(|| Some((metric.parse::<u32>().ok()?, iface.to_string())))
                .flatten()
        })
        .min()
        .map(|(_, iface)| iface)
}

/// This machine's IPv4 address on its default-route interface, if it has
/// one yet (DHCP may still be running).
pub fn detect() -> Option<IpAddr> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    let iface = default_route_interface(&routes)?;
    interface_ipv4(&iface).map(IpAddr::V4)
}

#[cfg(target_os = "linux")]
fn interface_ipv4(iface: &str) -> Option<Ipv4Addr> {
    nix::ifaddrs::getifaddrs()
        .ok()?
        .filter(|address| address.interface_name == iface)
        .find_map(|address| {
            address
                .address
                .and_then(|a| a.as_sockaddr_in().map(|v4| v4.ip()))
        })
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
}

#[cfg(not(target_os = "linux"))]
fn interface_ipv4(_iface: &str) -> Option<Ipv4Addr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
enp1s0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
enp1s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";

    #[test]
    fn the_lowest_metric_default_route_wins() {
        assert_eq!(default_route_interface(ROUTES).as_deref(), Some("enp1s0"));
    }

    #[test]
    fn no_default_route_means_no_interface() {
        let only_local = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                          enp1s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n";
        assert_eq!(default_route_interface(only_local), None);
        assert_eq!(default_route_interface(""), None);
        assert_eq!(default_route_interface("Iface\nbroken line\n"), None);
    }
}
